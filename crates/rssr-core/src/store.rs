use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::fetch::Validators;
use crate::opml::Subscription;
use crate::parse::ParsedItem;
use crate::{Error, Result};

const SCHEMA_V1: &str = r#"
CREATE TABLE feeds (
    id            INTEGER PRIMARY KEY,
    url           TEXT NOT NULL UNIQUE,
    title         TEXT,
    site_url      TEXT,
    folder        TEXT,
    etag          TEXT,
    last_modified TEXT,
    fetched_at    TEXT,
    status        TEXT,
    error         TEXT
);

CREATE TABLE items (
    id        TEXT PRIMARY KEY,
    feed_id   INTEGER NOT NULL REFERENCES feeds(id) ON DELETE CASCADE,
    id_source TEXT NOT NULL,
    guid      TEXT,
    url       TEXT,
    title     TEXT,
    author    TEXT,
    summary   TEXT,
    published TEXT,
    updated   TEXT,
    seen_at   TEXT NOT NULL,
    read      INTEGER NOT NULL DEFAULT 0,
    starred   INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX items_by_feed ON items(feed_id, published DESC);
CREATE INDEX items_unread ON items(read, published DESC);

CREATE TABLE bodies (
    item_id    TEXT PRIMARY KEY REFERENCES items(id) ON DELETE CASCADE,
    content    TEXT NOT NULL,
    format     TEXT NOT NULL,
    source     TEXT NOT NULL,
    fetched_at TEXT NOT NULL
);
"#;

/// The index stores no text of its own; `content=items` points it back at the
/// table, and the triggers keep the two in step.
const SCHEMA_V2: &str = r#"
CREATE VIRTUAL TABLE items_fts USING fts5(
    title,
    summary,
    content = 'items',
    content_rowid = 'rowid'
);

CREATE TRIGGER items_fts_insert AFTER INSERT ON items BEGIN
    INSERT INTO items_fts(rowid, title, summary)
    VALUES (new.rowid, new.title, new.summary);
END;

CREATE TRIGGER items_fts_delete AFTER DELETE ON items BEGIN
    INSERT INTO items_fts(items_fts, rowid, title, summary)
    VALUES ('delete', old.rowid, old.title, old.summary);
END;

CREATE TRIGGER items_fts_update AFTER UPDATE ON items BEGIN
    INSERT INTO items_fts(items_fts, rowid, title, summary)
    VALUES ('delete', old.rowid, old.title, old.summary);
    INSERT INTO items_fts(rowid, title, summary)
    VALUES (new.rowid, new.title, new.summary);
END;

INSERT INTO items_fts(rowid, title, summary)
SELECT rowid, title, summary FROM items;
"#;

#[derive(Debug, Clone)]
pub struct Feed {
    pub id: i64,
    pub url: String,
    pub title: Option<String>,
    pub folder: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub fetched_at: Option<DateTime<Utc>>,
    /// Scrape each item's page because this feed only publishes a teaser.
    pub full_content: bool,
    pub status: Option<String>,
    pub error: Option<String>,
    pub unread: i64,
}

#[derive(Debug, Clone)]
pub struct Item {
    pub id: String,
    pub feed_id: i64,
    pub feed_title: Option<String>,
    pub title: Option<String>,
    pub url: Option<String>,
    pub author: Option<String>,
    pub published: Option<String>,
    pub read: bool,
    pub starred: bool,
    /// Plain-text opening of the item, when the caller asked for one.
    pub snippet: Option<String>,
    /// Search relevance; lower is better. Only set by `search`.
    pub score: Option<f64>,
    /// Other copies of this item collapsed into it, when deduplicating.
    pub duplicates: usize,
}

#[derive(Debug, Clone)]
pub struct Body {
    pub item_id: String,
    pub feed_id: i64,
    pub feed_title: Option<String>,
    pub title: Option<String>,
    pub url: Option<String>,
    pub author: Option<String>,
    pub published: Option<String>,
    pub read: bool,
    pub starred: bool,
    pub content: String,
    pub format: String,
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct Query {
    pub unread_only: bool,
    pub starred_only: bool,
    pub feed_id: Option<i64>,
    pub folder: Option<String>,
    /// Only items dated at or after this instant.
    pub since: Option<DateTime<Utc>>,
    pub limit: usize,
    /// At most this many items from any one feed, so a busy feed cannot fill
    /// the whole page.
    pub per_feed: Option<usize>,
    /// Characters of plain-text preview to include with each item.
    pub snippet: Option<usize>,
    /// Collapse the same story arriving from several feeds into one row.
    pub dedupe: bool,
}

impl Default for Query {
    fn default() -> Self {
        Query {
            unread_only: true,
            starred_only: false,
            feed_id: None,
            folder: None,
            since: None,
            limit: 50,
            per_feed: None,
            snippet: None,
            dedupe: false,
        }
    }
}

impl Query {
    fn bindings(&self) -> [Box<dyn rusqlite::ToSql>; 5] {
        [
            Box::new(self.unread_only as i64),
            Box::new(self.feed_id),
            Box::new(self.folder.clone()),
            Box::new(self.starred_only as i64),
            Box::new(self.since.map(stamp)),
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upsert {
    Added,
    Updated,
    Unchanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flag {
    Read,
    Unread,
    Star,
    Unstar,
}

impl Flag {
    fn assignment(self) -> &'static str {
        match self {
            Flag::Read => "read = 1",
            Flag::Unread => "read = 0",
            Flag::Star => "starred = 1",
            Flag::Unstar => "starred = 0",
        }
    }
}

/// Filters are written as `?n IS NULL OR ...` so one prepared statement
/// serves every combination instead of pasting SQL together at runtime.
const SCHEMA_V3: &str = r#"
ALTER TABLE feeds ADD COLUMN full_content INTEGER NOT NULL DEFAULT 0;
"#;

const LATEST_VERSION: i64 = 3;

const FILTER: &str = "WHERE (?1 = 0 OR items.read = 0)
              AND (?2 IS NULL OR items.feed_id = ?2)
              AND (?3 IS NULL OR feeds.folder = ?3)
              AND (?4 = 0 OR items.starred = 1)
              AND (?5 IS NULL OR COALESCE(items.published, items.updated, items.seen_at) >= ?5)";

const COLUMNS: &str = "items.id, items.feed_id, feeds.title, items.title, items.url,
                       items.author,
                       COALESCE(items.published, items.updated, items.seen_at) AS at,
                       items.read, items.starred";

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub feeds: i64,
    pub folders: i64,
    pub items: i64,
    pub unread: i64,
    pub starred: i64,
    pub full_text: i64,
    pub failing: i64,
    pub last_refresh: Option<String>,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        Self::from_connection(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA temp_store = MEMORY;
             PRAGMA cache_size = -65536;",
        )?;
        let store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let version: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        for (target, statements) in [(1, SCHEMA_V1), (2, SCHEMA_V2), (3, SCHEMA_V3)] {
            if version < target {
                self.conn.execute_batch(statements)?;
            }
        }
        self.conn
            .execute_batch(&format!("PRAGMA user_version = {LATEST_VERSION}"))?;
        Ok(())
    }

    /// Adds a feed. A feed already subscribed keeps the title and folder it
    /// has unless `overwrite` is set: an import should not silently rename
    /// feeds or move them between folders.
    pub fn upsert_feed(&self, sub: &Subscription, overwrite: bool) -> Result<(i64, Upsert)> {
        if let Some(id) = self.feed_id(&sub.url)? {
            if !overwrite {
                return Ok((id, Upsert::Unchanged));
            }
            let changed = self.conn.execute(
                "UPDATE feeds SET
                     title  = COALESCE(?2, title),
                     folder = COALESCE(?3, folder)
                 WHERE id = ?1 AND (title IS NOT ?2 OR folder IS NOT ?3)",
                params![id, sub.title, sub.folder],
            )?;
            return Ok((
                id,
                if changed > 0 {
                    Upsert::Updated
                } else {
                    Upsert::Unchanged
                },
            ));
        }

        self.conn.execute(
            "INSERT INTO feeds (url, title, folder) VALUES (?1, ?2, ?3)",
            params![sub.url, sub.title, sub.folder],
        )?;
        let id = self
            .feed_id(&sub.url)?
            .ok_or_else(|| Error::Config(format!("feed vanished after insert: {}", sub.url)))?;
        Ok((id, Upsert::Added))
    }

    /// Unsubscribes and drops everything stored for the feed.
    pub fn remove_feed(&self, feed_id: i64) -> Result<bool> {
        let removed = self
            .conn
            .execute("DELETE FROM feeds WHERE id = ?1", [feed_id])?;
        Ok(removed > 0)
    }

    pub fn feed_exists(&self, feed_id: i64) -> Result<bool> {
        Ok(self
            .conn
            .query_row("SELECT 1 FROM feeds WHERE id = ?1", [feed_id], |_| Ok(()))
            .optional()?
            .is_some())
    }

    pub fn folders(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT folder FROM feeds WHERE folder IS NOT NULL ORDER BY folder",
        )?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn feed_id(&self, url: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT id FROM feeds WHERE url = ?1", [url], |row| {
                row.get(0)
            })
            .optional()?)
    }

    pub fn feeds(&self) -> Result<Vec<Feed>> {
        let mut stmt = self.conn.prepare(
            "SELECT feeds.id, feeds.url, feeds.title, feeds.folder, feeds.etag,
                    feeds.last_modified, feeds.fetched_at, feeds.full_content,
                    feeds.status, feeds.error,
                    COUNT(items.id) FILTER (WHERE items.read = 0)
             FROM feeds LEFT JOIN items ON items.feed_id = feeds.id
             GROUP BY feeds.id
             ORDER BY feeds.folder IS NULL, feeds.folder, feeds.title, feeds.url",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Feed {
                id: row.get(0)?,
                url: row.get(1)?,
                title: row.get(2)?,
                folder: row.get(3)?,
                etag: row.get(4)?,
                last_modified: row.get(5)?,
                fetched_at: row.get::<_, Option<String>>(6)?.and_then(parse_stamp),
                full_content: row.get::<_, i64>(7)? != 0,
                status: row.get(8)?,
                error: row.get(9)?,
                unread: row.get(10)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn update_feed_meta(
        &self,
        feed_id: i64,
        title: Option<&str>,
        site_url: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE feeds SET
                 title    = COALESCE(title, ?2),
                 site_url = COALESCE(?3, site_url)
             WHERE id = ?1",
            params![feed_id, title, site_url],
        )?;
        Ok(())
    }

    pub fn record_fetch(
        &self,
        feed_id: i64,
        status: &str,
        validators: Option<&Validators>,
        error: Option<&str>,
    ) -> Result<()> {
        let (etag, last_modified) = match validators {
            Some(v) => (v.etag.as_deref(), v.last_modified.as_deref()),
            None => (None, None),
        };
        self.conn.execute(
            "UPDATE feeds SET
                 fetched_at    = ?2,
                 status        = ?3,
                 error         = ?4,
                 etag          = COALESCE(?5, etag),
                 last_modified = COALESCE(?6, last_modified)
             WHERE id = ?1",
            params![feed_id, now(), status, error, etag, last_modified],
        )?;
        Ok(())
    }

    /// Writes a whole fetch in one transaction and reports how many items were
    /// new. Items already stored keep their read and starred flags; only the
    /// fields the publisher can edit are refreshed.
    pub fn save_items(&mut self, feed_id: i64, items: &[ParsedItem]) -> Result<usize> {
        let seen_at = now();
        let tx = self.conn.transaction()?;
        let mut new = 0;
        {
            let mut insert = tx.prepare(
                "INSERT OR IGNORE INTO items
                     (id, feed_id, id_source, guid, url, title, author, summary,
                      published, updated, seen_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?;
            let mut update = tx.prepare(
                "UPDATE items SET
                     url = ?2, title = ?3, author = ?4, summary = ?5, updated = ?6
                 WHERE id = ?1",
            )?;
            let mut body = tx.prepare(
                "INSERT INTO bodies (item_id, content, format, source, fetched_at)
                 VALUES (?1, ?2, 'html', 'feed', ?3)
                 ON CONFLICT(item_id) DO UPDATE SET content = excluded.content
                 WHERE bodies.source = 'feed'",
            )?;

            for item in items {
                let published = item.published.map(stamp);
                let updated = item.updated.map(stamp);
                let rows = insert.execute(params![
                    item.id,
                    feed_id,
                    item.id_source.as_str(),
                    item.guid,
                    item.url,
                    item.title,
                    item.author,
                    item.summary,
                    published,
                    updated,
                    seen_at,
                ])?;
                if rows == 0 {
                    update.execute(params![
                        item.id,
                        item.url,
                        item.title,
                        item.author,
                        item.summary,
                        updated,
                    ])?;
                } else {
                    new += 1;
                }
                if let Some(content) = &item.content {
                    body.execute(params![item.id, content, seen_at])?;
                }
            }
        }
        tx.commit()?;
        Ok(new)
    }

    pub fn items(&self, query: &Query) -> Result<Vec<Item>> {
        // `per_feed` needs more rows than asked for, since the cap is applied
        // after ordering; without a limit at all a large database would be
        // read end to end.
        let fetch = match (query.per_feed, query.dedupe) {
            (None, false) => query.limit,
            _ => (query.limit * 20).max(500),
        };
        let sql = format!(
            "SELECT {COLUMNS}, COALESCE(bodies.content, items.summary)
             FROM items
             JOIN feeds ON feeds.id = items.feed_id
             LEFT JOIN bodies ON bodies.item_id = items.id
             {FILTER}
             ORDER BY at DESC
             LIMIT ?6"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let bindings = query.bindings();
        let mut params: Vec<&dyn rusqlite::ToSql> =
            bindings.iter().map(|value| value.as_ref()).collect();
        let fetch = fetch as i64;
        params.push(&fetch);

        let rows = stmt.query_map(params.as_slice(), |row| {
            let mut item = read_item(row)?;
            item.snippet = query.snippet.and_then(|width| {
                row.get::<_, Option<String>>(9)
                    .ok()
                    .flatten()
                    .map(|text| crate::content::preview(&text, width))
            });
            Ok(item)
        })?;
        let items = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        let items = collapse_duplicates(items, query);
        let mut items = cap_per_feed(items, query);
        items.truncate(query.limit);
        Ok(items)
    }

    /// How many items the query matches in total, so a caller never has to
    /// page just to learn the size of the result.
    pub fn count(&self, query: &Query) -> Result<i64> {
        let sql =
            format!("SELECT COUNT(*) FROM items JOIN feeds ON feeds.id = items.feed_id {FILTER}");
        let bindings = query.bindings();
        let params: Vec<&dyn rusqlite::ToSql> =
            bindings.iter().map(|value| value.as_ref()).collect();
        Ok(self
            .conn
            .query_row(&sql, params.as_slice(), |row| row.get(0))?)
    }

    /// Full-text search, honouring the same filters as `items` so a caller can
    /// scope a query to one feed, folder, or time window.
    pub fn search(&self, text: &str, query: &Query) -> Result<Vec<Item>> {
        let sql = format!(
            "SELECT {COLUMNS},
                    snippet(items_fts, -1, '', '', '…', 24),
                    bm25(items_fts)
             FROM items_fts
             JOIN items ON items.rowid = items_fts.rowid
             JOIN feeds ON feeds.id = items.feed_id
             {FILTER} AND items_fts MATCH ?6
             ORDER BY bm25(items_fts)
             LIMIT ?7"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let bindings = query.bindings();
        let mut params: Vec<&dyn rusqlite::ToSql> =
            bindings.iter().map(|value| value.as_ref()).collect();
        let limit = query.limit as i64;
        params.push(&text);
        params.push(&limit);

        let rows = stmt.query_map(params.as_slice(), |row| {
            let mut item = read_item(row)?;
            // FTS5 hands back the indexed text as stored, which for a summary
            // column means raw markup. Same cleaning as `items` does.
            item.snippet = row
                .get::<_, Option<String>>(9)?
                .map(|text| crate::content::plain_text(&text));
            item.score = row.get::<_, Option<f64>>(10)?;
            Ok(item)
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn set_full_content(&self, feed_id: i64, on: bool) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE feeds SET full_content = ?2 WHERE id = ?1",
            params![feed_id, on as i64],
        )?;
        Ok(changed > 0)
    }

    pub fn save_body(&self, item_id: &str, content: &str, source: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO bodies (item_id, content, format, source, fetched_at)
             VALUES (?1, ?2, 'html', ?3, ?4)
             ON CONFLICT(item_id) DO UPDATE SET
                 content    = excluded.content,
                 source     = excluded.source,
                 fetched_at = excluded.fetched_at",
            params![item_id, content, source, now()],
        )?;
        Ok(())
    }

    /// Items on a feed whose page has not been scraped yet, newest first.
    pub fn awaiting_extraction(&self, feed_id: i64, limit: usize) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT items.id, items.url
             FROM items LEFT JOIN bodies ON bodies.item_id = items.id
             WHERE items.feed_id = ?1
               AND items.url IS NOT NULL
               AND (bodies.source IS NULL OR bodies.source <> 'extracted')
             ORDER BY COALESCE(items.published, items.updated, items.seen_at) DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![feed_id, limit as i64], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn item_url(&self, item_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT url FROM items WHERE id = ?1", [item_id], |row| {
                row.get(0)
            })
            .optional()?)
    }

    pub fn body(&self, item_id: &str) -> Result<Option<Body>> {
        Ok(self
            .conn
            .query_row(
                "SELECT items.id, items.feed_id, feeds.title, items.title, items.url,
                        items.author,
                        COALESCE(items.published, items.updated, items.seen_at),
                        items.read, items.starred,
                        COALESCE(bodies.content, items.summary, ''),
                        COALESCE(bodies.format, 'html'),
                        COALESCE(bodies.source, 'summary')
                 FROM items
                 JOIN feeds ON feeds.id = items.feed_id
                 LEFT JOIN bodies ON bodies.item_id = items.id
                 WHERE items.id = ?1",
                [item_id],
                |row| {
                    Ok(Body {
                        item_id: row.get(0)?,
                        feed_id: row.get(1)?,
                        feed_title: row.get(2)?,
                        title: row.get(3)?,
                        url: row.get(4)?,
                        author: row.get(5)?,
                        published: row.get(6)?,
                        read: row.get::<_, i64>(7)? != 0,
                        starred: row.get::<_, i64>(8)? != 0,
                        content: row.get(9)?,
                        format: row.get(10)?,
                        source: row.get(11)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn set_flag(&self, ids: &[String], flag: Flag) -> Result<usize> {
        let sql = format!("UPDATE items SET {} WHERE id = ?1", flag.assignment());
        let mut stmt = self.conn.prepare(&sql)?;
        let mut changed = 0;
        for id in ids {
            changed += stmt.execute([id])?;
        }
        Ok(changed)
    }

    /// One round trip for everything the status view needs.
    pub fn stats(&self) -> Result<Stats> {
        Ok(self.conn.query_row(
            "SELECT
                 (SELECT COUNT(*) FROM feeds),
                 (SELECT COUNT(DISTINCT folder) FROM feeds WHERE folder IS NOT NULL),
                 (SELECT COUNT(*) FROM items),
                 (SELECT COUNT(*) FROM items WHERE read = 0),
                 (SELECT COUNT(*) FROM items WHERE starred = 1),
                 (SELECT COUNT(*) FROM bodies WHERE source = 'extracted'),
                 (SELECT COUNT(*) FROM feeds WHERE status = 'error'),
                 (SELECT MAX(fetched_at) FROM feeds)",
            [],
            |row| {
                Ok(Stats {
                    feeds: row.get(0)?,
                    folders: row.get(1)?,
                    items: row.get(2)?,
                    unread: row.get(3)?,
                    starred: row.get(4)?,
                    full_text: row.get(5)?,
                    failing: row.get(6)?,
                    last_refresh: row.get(7)?,
                })
            },
        )?)
    }

    pub fn unread_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM items WHERE read = 0", [], |row| {
                row.get(0)
            })?)
    }

    pub fn default_path() -> Result<PathBuf> {
        let dir = dirs::data_dir()
            .ok_or_else(|| Error::Config("no data directory for this platform".into()))?;
        Ok(dir.join("rssr").join("rssr.db"))
    }
}

fn parse_stamp(value: String) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&value)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

fn read_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<Item> {
    Ok(Item {
        id: row.get(0)?,
        feed_id: row.get(1)?,
        feed_title: row.get(2)?,
        title: row.get(3)?,
        url: row.get(4)?,
        author: row.get(5)?,
        published: row.get(6)?,
        read: row.get::<_, i64>(7)? != 0,
        starred: row.get::<_, i64>(8)? != 0,
        snippet: None,
        score: None,
        duplicates: 0,
    })
}

/// The same story often arrives from several feeds under different ids, since
/// an id is namespaced by the feed it came from. Matching on the cleaned link,
/// then on the exact title, catches both the syndicated copy and the aggregator
/// that rewrites every link.
fn collapse_duplicates(items: Vec<Item>, query: &Query) -> Vec<Item> {
    if !query.dedupe {
        return items;
    }
    let mut first_seen: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<Item> = Vec::new();
    for item in items {
        let keys = dedupe_keys(&item);
        match keys.iter().find_map(|key| first_seen.get(key).copied()) {
            Some(index) => out[index].duplicates += 1,
            None => {
                for key in keys {
                    first_seen.insert(key, out.len());
                }
                out.push(item);
            }
        }
    }
    out
}

fn dedupe_keys(item: &Item) -> Vec<String> {
    let mut keys = Vec::new();
    if let Some(url) = &item.url {
        keys.push(format!("url:{}", crate::identity::normalize_url(url)));
    }
    if let Some(title) = &item.title {
        let title = title.trim().to_lowercase();
        if !title.is_empty() {
            keys.push(format!("title:{title}"));
        }
    }
    keys
}

/// Keeps the newest `per_feed` items from each feed, in the order they were
/// already sorted, then trims to the requested limit.
fn cap_per_feed(items: Vec<Item>, query: &Query) -> Vec<Item> {
    let Some(per_feed) = query.per_feed else {
        return items;
    };
    let mut taken: HashMap<i64, usize> = HashMap::new();
    items
        .into_iter()
        .filter(|item| {
            let count = taken.entry(item.feed_id).or_default();
            *count += 1;
            *count <= per_feed
        })
        .collect()
}

fn now() -> String {
    stamp(Utc::now())
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::IdSource;

    /// Every item, regardless of read state, for tests that just want rows.
    fn any() -> Query {
        Query {
            unread_only: false,
            ..Query::default()
        }
    }

    fn add(store: &Store, url: &str, folder: Option<&str>) -> i64 {
        store.upsert_feed(&sub(url, folder), false).unwrap().0
    }

    fn sub(url: &str, folder: Option<&str>) -> Subscription {
        Subscription {
            url: url.into(),
            title: Some("Title".into()),
            folder: folder.map(Into::into),
        }
    }

    #[test]
    fn upsert_is_idempotent() {
        let store = Store::open_in_memory().unwrap();
        let first = add(&store, "https://a.com/feed", None);
        let again = add(&store, "https://a.com/feed", None);
        assert_eq!(first, again);
        assert_eq!(store.feeds().unwrap().len(), 1);
    }

    fn item(id: &str, title: &str) -> ParsedItem {
        ParsedItem {
            id: id.into(),
            id_source: IdSource::Guid,
            guid: Some(id.into()),
            url: Some(format!("https://a.com/{id}")),
            title: Some(title.into()),
            author: None,
            summary: None,
            content: None,
            published: Some(Utc::now()),
            updated: None,
        }
    }

    #[test]
    fn only_unseen_items_count_as_new() {
        let mut store = Store::open_in_memory().unwrap();
        let feed = add(&store, "https://a.com/feed", None);

        let first = vec![item("a", "One"), item("b", "Two")];
        assert_eq!(store.save_items(feed, &first).unwrap(), 2);

        let second = vec![item("b", "Two"), item("c", "Three")];
        assert_eq!(store.save_items(feed, &second).unwrap(), 1);
        assert_eq!(store.unread_count().unwrap(), 3);
    }

    #[test]
    fn a_since_filter_hides_older_items() {
        let mut store = Store::open_in_memory().unwrap();
        let feed = add(&store, "https://a.com/feed", None);
        let mut old = item("old", "Last year");
        old.published = Some(Utc::now() - chrono::Duration::days(400));
        store
            .save_items(feed, &[old, item("new", "Today")])
            .unwrap();

        let query = Query {
            since: Some(Utc::now() - chrono::Duration::days(1)),
            ..Query::default()
        };
        let found = store.items(&query).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "new");
        assert_eq!(store.count(&query).unwrap(), 1);
    }

    #[test]
    fn starred_only_returns_what_was_starred() {
        let mut store = Store::open_in_memory().unwrap();
        let feed = add(&store, "https://a.com/feed", None);
        store
            .save_items(feed, &[item("a", "One"), item("b", "Two")])
            .unwrap();
        store.set_flag(&["b".to_string()], Flag::Star).unwrap();

        let query = Query {
            starred_only: true,
            unread_only: false,
            ..Query::default()
        };
        let found = store.items(&query).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "b");
    }

    #[test]
    fn one_busy_feed_cannot_fill_the_page() {
        let mut store = Store::open_in_memory().unwrap();
        let busy = add(&store, "https://busy.com/feed", None);
        let quiet = add(&store, "https://quiet.com/feed", None);
        let flood: Vec<_> = (0..10)
            .map(|n| item(&format!("busy{n}"), "Flood"))
            .collect();
        store.save_items(busy, &flood).unwrap();
        store.save_items(quiet, &[item("quiet1", "Rare")]).unwrap();

        let query = Query {
            per_feed: Some(2),
            limit: 10,
            ..Query::default()
        };
        let found = store.items(&query).unwrap();
        assert_eq!(found.len(), 3);
        assert_eq!(found.iter().filter(|i| i.feed_id == busy).count(), 2);
        assert_eq!(found.iter().filter(|i| i.feed_id == quiet).count(), 1);
    }

    #[test]
    fn a_snippet_is_plain_text_and_bounded() {
        let mut store = Store::open_in_memory().unwrap();
        let feed = add(&store, "https://a.com/feed", None);
        let mut long = item("a", "One");
        long.content = Some(format!("<p>{}</p>", "word ".repeat(200)));
        store.save_items(feed, &[long]).unwrap();

        let query = Query {
            snippet: Some(40),
            ..Query::default()
        };
        let snippet = store.items(&query).unwrap()[0].snippet.clone().unwrap();
        assert!(!snippet.contains('<'));
        assert!(snippet.chars().count() <= 41);
    }

    #[test]
    fn search_can_be_scoped_and_comes_back_ranked() {
        let mut store = Store::open_in_memory().unwrap();
        let rust = add(&store, "https://a.com/feed", Some("Rust"));
        let other = add(&store, "https://b.com/feed", Some("Go"));
        store
            .save_items(rust, &[item("a", "Async Rust in practice")])
            .unwrap();
        store
            .save_items(other, &[item("b", "Rust versus Go")])
            .unwrap();

        let scoped = Query {
            unread_only: false,
            folder: Some("Rust".into()),
            ..Query::default()
        };
        let found = store.search("rust", &scoped).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "a");
        assert!(found[0].score.is_some());
        assert!(found[0].snippet.is_some());
    }

    #[test]
    fn search_snippets_are_plain_text_like_list_snippets() {
        let mut store = Store::open_in_memory().unwrap();
        let feed = add(&store, "https://a.com/feed", None);
        let mut marked_up = item("a", "Token optimization");
        marked_up.summary =
            Some("<p>Context window <b>optimization</b>&nbsp;for agents</p>".into());
        store.save_items(feed, &[marked_up]).unwrap();

        let snippet = store.search("optimization", &any()).unwrap()[0]
            .snippet
            .clone()
            .unwrap();
        assert!(!snippet.contains('<'), "{snippet}");
        assert!(!snippet.contains("&nbsp;"), "{snippet}");
    }

    #[test]
    fn one_story_from_two_feeds_collapses_into_one_row() {
        let mut store = Store::open_in_memory().unwrap();
        let one = add(&store, "https://a.com/feed", None);
        let two = add(&store, "https://b.com/feed", None);

        let mut here = item("here", "GPT-6 Astra announced");
        here.url = Some("https://openai.com/astra".into());
        let mut there = item("there", "GPT-6 Astra announced");
        there.url = Some("https://openai.com/astra?utm_source=rss".into());
        store.save_items(one, &[here]).unwrap();
        store.save_items(two, &[there]).unwrap();

        let plain = store.items(&Query::default()).unwrap();
        assert_eq!(plain.len(), 2);

        let merged = store
            .items(&Query {
                dedupe: true,
                ..Query::default()
            })
            .unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].duplicates, 1);
    }

    #[test]
    fn the_limit_still_holds_once_duplicates_are_collapsed() {
        let mut store = Store::open_in_memory().unwrap();
        let feed = add(&store, "https://a.com/feed", None);
        let many: Vec<_> = (0..30)
            .map(|n| item(&format!("i{n}"), &format!("Story {n}")))
            .collect();
        store.save_items(feed, &many).unwrap();

        let page = store
            .items(&Query {
                dedupe: true,
                limit: 4,
                ..Query::default()
            })
            .unwrap();
        assert_eq!(page.len(), 4);
    }

    #[test]
    fn deduping_keeps_genuinely_different_stories_apart() {
        let mut store = Store::open_in_memory().unwrap();
        let feed = add(&store, "https://a.com/feed", None);
        store
            .save_items(feed, &[item("a", "One"), item("b", "Two")])
            .unwrap();

        let merged = store
            .items(&Query {
                dedupe: true,
                ..Query::default()
            })
            .unwrap();
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn stats_describe_an_empty_database_without_failing() {
        let store = Store::open_in_memory().unwrap();
        let stats = store.stats().unwrap();
        assert_eq!(stats.feeds, 0);
        assert_eq!(stats.items, 0);
        assert_eq!(stats.last_refresh, None);
    }

    #[test]
    fn stats_count_what_is_there() {
        let mut store = Store::open_in_memory().unwrap();
        let feed = add(&store, "https://a.com/feed", Some("Tech"));
        store
            .save_items(feed, &[item("a", "One"), item("b", "Two")])
            .unwrap();
        store.set_flag(&["a".to_string()], Flag::Read).unwrap();
        store.save_body("b", "<p>full</p>", "extracted").unwrap();

        let stats = store.stats().unwrap();
        assert_eq!((stats.feeds, stats.folders), (1, 1));
        assert_eq!((stats.items, stats.unread), (2, 1));
        assert_eq!(stats.full_text, 1);
    }

    #[test]
    fn listing_defaults_to_unread_and_reports_the_total() {
        let mut store = Store::open_in_memory().unwrap();
        let feed = add(&store, "https://a.com/feed", Some("Tech"));
        store
            .save_items(feed, &[item("a", "One"), item("b", "Two")])
            .unwrap();
        store.set_flag(&["a".to_string()], Flag::Read).unwrap();

        let query = Query::default();
        assert_eq!(store.count(&query).unwrap(), 1);
        assert_eq!(store.items(&query).unwrap()[0].id, "b");

        let all = Query {
            unread_only: false,
            ..Query::default()
        };
        assert_eq!(store.count(&all).unwrap(), 2);
    }

    #[test]
    fn an_extracted_body_replaces_the_feed_one_and_survives_a_refresh() {
        let mut store = Store::open_in_memory().unwrap();
        let feed = add(&store, "https://a.com/feed", None);
        let mut teaser = item("a", "One");
        teaser.content = Some("<p>Teaser</p>".into());
        store.save_items(feed, &[teaser.clone()]).unwrap();

        store
            .save_body("a", "<p>The whole article</p>", "extracted")
            .unwrap();
        store.save_items(feed, &[teaser]).unwrap();

        let body = store.body("a").unwrap().unwrap();
        assert_eq!(body.content, "<p>The whole article</p>");
        assert_eq!(body.source, "extracted");
    }

    #[test]
    fn only_unscraped_items_are_queued_for_extraction() {
        let mut store = Store::open_in_memory().unwrap();
        let feed = add(&store, "https://a.com/feed", None);
        store
            .save_items(feed, &[item("a", "One"), item("b", "Two")])
            .unwrap();
        store.save_body("a", "<p>Done</p>", "extracted").unwrap();

        let queued = store.awaiting_extraction(feed, 10).unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].0, "b");
    }

    #[test]
    fn a_feed_supplied_body_is_stored_and_read_back() {
        let mut store = Store::open_in_memory().unwrap();
        let feed = add(&store, "https://a.com/feed", None);
        let mut with_body = item("a", "One");
        with_body.content = Some("<p>Hello</p>".into());
        store.save_items(feed, &[with_body]).unwrap();

        let body = store.body("a").unwrap().unwrap();
        assert_eq!(body.content, "<p>Hello</p>");
        assert_eq!(body.source, "feed");
    }

    #[test]
    fn an_item_without_a_body_falls_back_to_its_summary() {
        let mut store = Store::open_in_memory().unwrap();
        let feed = add(&store, "https://a.com/feed", None);
        let mut summarised = item("a", "One");
        summarised.summary = Some("Just a teaser".into());
        store.save_items(feed, &[summarised]).unwrap();

        let body = store.body("a").unwrap().unwrap();
        assert_eq!(body.content, "Just a teaser");
        assert_eq!(body.source, "summary");
    }

    #[test]
    fn search_finds_items_by_title() {
        let mut store = Store::open_in_memory().unwrap();
        let feed = add(&store, "https://a.com/feed", None);
        store
            .save_items(
                feed,
                &[item("a", "Async Rust in practice"), item("b", "Go modules")],
            )
            .unwrap();

        let found = store.search("rust", &any()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "a");
    }

    #[test]
    fn the_index_follows_an_edited_title() {
        let mut store = Store::open_in_memory().unwrap();
        let feed = add(&store, "https://a.com/feed", None);
        store
            .save_items(feed, &[item("a", "Draft heading")])
            .unwrap();
        store
            .save_items(feed, &[item("a", "Published heading")])
            .unwrap();

        assert!(store.search("draft", &any()).unwrap().is_empty());
        assert_eq!(store.search("published", &any()).unwrap().len(), 1);
    }

    #[test]
    fn a_folder_filter_only_matches_its_own_feeds() {
        let mut store = Store::open_in_memory().unwrap();
        let tech = add(&store, "https://a.com/feed", Some("Tech"));
        let news = add(&store, "https://b.com/feed", Some("News"));
        store.save_items(tech, &[item("a", "One")]).unwrap();
        store.save_items(news, &[item("b", "Two")]).unwrap();

        let query = Query {
            folder: Some("Tech".into()),
            ..Query::default()
        };
        let found = store.items(&query).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "a");
    }

    #[test]
    fn marking_something_twice_changes_nothing_the_second_time() {
        let mut store = Store::open_in_memory().unwrap();
        let feed = add(&store, "https://a.com/feed", None);
        store.save_items(feed, &[item("a", "One")]).unwrap();

        let ids = vec!["a".to_string()];
        assert_eq!(store.set_flag(&ids, Flag::Read).unwrap(), 1);
        store.set_flag(&ids, Flag::Read).unwrap();
        assert_eq!(store.unread_count().unwrap(), 0);
    }

    #[test]
    fn an_edited_item_keeps_its_read_flag() {
        let mut store = Store::open_in_memory().unwrap();
        let feed = add(&store, "https://a.com/feed", None);
        store.save_items(feed, &[item("a", "Draft")]).unwrap();
        store
            .conn
            .execute("UPDATE items SET read = 1 WHERE id = 'a'", [])
            .unwrap();

        store.save_items(feed, &[item("a", "Published")]).unwrap();

        let (title, read): (String, i64) = store
            .conn
            .query_row("SELECT title, read FROM items WHERE id = 'a'", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(title, "Published");
        assert_eq!(read, 1);
    }

    #[test]
    fn a_reimport_leaves_an_existing_title_and_folder_alone() {
        let store = Store::open_in_memory().unwrap();
        add(&store, "https://a.com/feed", Some("Tech"));

        let renamed = Subscription {
            url: "https://a.com/feed".into(),
            title: Some("Renamed".into()),
            folder: Some("Elsewhere".into()),
        };
        let (_, outcome) = store.upsert_feed(&renamed, false).unwrap();

        assert_eq!(outcome, Upsert::Unchanged);
        let feed = &store.feeds().unwrap()[0];
        assert_eq!(feed.title.as_deref(), Some("Title"));
        assert_eq!(feed.folder.as_deref(), Some("Tech"));
    }

    #[test]
    fn overwriting_a_feed_label_has_to_be_asked_for() {
        let store = Store::open_in_memory().unwrap();
        add(&store, "https://a.com/feed", Some("Tech"));

        let renamed = Subscription {
            url: "https://a.com/feed".into(),
            title: Some("Renamed".into()),
            folder: Some("Elsewhere".into()),
        };
        let (_, outcome) = store.upsert_feed(&renamed, true).unwrap();

        assert_eq!(outcome, Upsert::Updated);
        let feed = &store.feeds().unwrap()[0];
        assert_eq!(feed.title.as_deref(), Some("Renamed"));
        assert_eq!(feed.folder.as_deref(), Some("Elsewhere"));
    }

    #[test]
    fn a_new_feed_reports_that_it_was_added() {
        let store = Store::open_in_memory().unwrap();
        let (_, first) = store
            .upsert_feed(&sub("https://a.com/feed", None), false)
            .unwrap();
        let (_, again) = store
            .upsert_feed(&sub("https://a.com/feed", None), false)
            .unwrap();
        assert_eq!(first, Upsert::Added);
        assert_eq!(again, Upsert::Unchanged);
    }

    #[test]
    fn removing_a_feed_takes_its_items_bodies_and_index_with_it() {
        let mut store = Store::open_in_memory().unwrap();
        let feed = add(&store, "https://a.com/feed", None);
        store
            .save_items(feed, &[item("a", "Unmistakable")])
            .unwrap();
        store.save_body("a", "<p>text</p>", "extracted").unwrap();

        assert!(store.remove_feed(feed).unwrap());
        assert!(!store.remove_feed(feed).unwrap());

        let stats = store.stats().unwrap();
        assert_eq!((stats.feeds, stats.items), (0, 0));
        assert!(store.body("a").unwrap().is_none());
        assert!(store.search("Unmistakable", &any()).unwrap().is_empty());
    }

    #[test]
    fn folders_are_listed_for_a_caller_that_needs_valid_names() {
        let store = Store::open_in_memory().unwrap();
        add(&store, "https://a.com/feed", Some("Tech"));
        add(&store, "https://b.com/feed", Some("News"));
        add(&store, "https://c.com/feed", None);
        assert_eq!(store.folders().unwrap(), vec!["News", "Tech"]);
    }
}
