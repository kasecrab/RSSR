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
}

#[derive(Debug, Clone)]
pub struct Item {
    pub id: String,
    pub feed_title: Option<String>,
    pub title: Option<String>,
    pub url: Option<String>,
    pub author: Option<String>,
    pub published: Option<String>,
    pub read: bool,
    pub starred: bool,
}

#[derive(Debug, Clone)]
pub struct Body {
    pub item_id: String,
    pub title: Option<String>,
    pub url: Option<String>,
    pub content: String,
    pub format: String,
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct Query {
    pub unread_only: bool,
    pub feed_id: Option<i64>,
    pub folder: Option<String>,
    pub limit: usize,
}

impl Default for Query {
    fn default() -> Self {
        Query {
            unread_only: true,
            feed_id: None,
            folder: None,
            limit: 50,
        }
    }
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
const LATEST_VERSION: i64 = 2;

const FILTER: &str = "WHERE (?1 = 0 OR items.read = 0)
              AND (?2 IS NULL OR items.feed_id = ?2)
              AND (?3 IS NULL OR feeds.folder = ?3)";

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
        for (target, statements) in [(1, SCHEMA_V1), (2, SCHEMA_V2)] {
            if version < target {
                self.conn.execute_batch(statements)?;
            }
        }
        self.conn
            .execute_batch(&format!("PRAGMA user_version = {LATEST_VERSION}"))?;
        Ok(())
    }

    /// Adds a feed, or refreshes the labels of one already subscribed.
    /// Cache validators are left alone so an import does not force a refetch.
    pub fn upsert_feed(&self, sub: &Subscription) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO feeds (url, title, folder) VALUES (?1, ?2, ?3)
             ON CONFLICT(url) DO UPDATE SET
                 title  = COALESCE(excluded.title, feeds.title),
                 folder = COALESCE(excluded.folder, feeds.folder)",
            params![sub.url, sub.title, sub.folder],
        )?;
        self.feed_id(&sub.url)?
            .ok_or_else(|| Error::Config(format!("feed vanished after insert: {}", sub.url)))
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
            "SELECT id, url, title, folder, etag, last_modified
             FROM feeds ORDER BY folder IS NULL, folder, title, url",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Feed {
                id: row.get(0)?,
                url: row.get(1)?,
                title: row.get(2)?,
                folder: row.get(3)?,
                etag: row.get(4)?,
                last_modified: row.get(5)?,
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
        let sql = format!(
            "SELECT items.id, feeds.title, items.title, items.url, items.author,
                    COALESCE(items.published, items.updated, items.seen_at) AS at,
                    items.read, items.starred
             FROM items JOIN feeds ON feeds.id = items.feed_id
             {FILTER}
             ORDER BY at DESC
             LIMIT ?4"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![
                query.unread_only as i64,
                query.feed_id,
                query.folder,
                query.limit as i64
            ],
            read_item,
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// How many items the query matches in total, so a caller never has to
    /// page just to learn the size of the result.
    pub fn count(&self, query: &Query) -> Result<i64> {
        let sql =
            format!("SELECT COUNT(*) FROM items JOIN feeds ON feeds.id = items.feed_id {FILTER}");
        Ok(self.conn.query_row(
            &sql,
            params![query.unread_only as i64, query.feed_id, query.folder],
            |row| row.get(0),
        )?)
    }

    pub fn search(&self, text: &str, limit: usize) -> Result<Vec<Item>> {
        let mut stmt = self.conn.prepare(
            "SELECT items.id, feeds.title, items.title, items.url, items.author,
                    COALESCE(items.published, items.updated, items.seen_at),
                    items.read, items.starred
             FROM items_fts
             JOIN items ON items.rowid = items_fts.rowid
             JOIN feeds ON feeds.id = items.feed_id
             WHERE items_fts MATCH ?1
             ORDER BY bm25(items_fts)
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![text, limit as i64], read_item)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn body(&self, item_id: &str) -> Result<Option<Body>> {
        Ok(self
            .conn
            .query_row(
                "SELECT items.id, items.title, items.url,
                        COALESCE(bodies.content, items.summary, ''),
                        COALESCE(bodies.format, 'html'),
                        COALESCE(bodies.source, 'summary')
                 FROM items LEFT JOIN bodies ON bodies.item_id = items.id
                 WHERE items.id = ?1",
                [item_id],
                |row| {
                    Ok(Body {
                        item_id: row.get(0)?,
                        title: row.get(1)?,
                        url: row.get(2)?,
                        content: row.get(3)?,
                        format: row.get(4)?,
                        source: row.get(5)?,
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

fn read_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<Item> {
    Ok(Item {
        id: row.get(0)?,
        feed_title: row.get(1)?,
        title: row.get(2)?,
        url: row.get(3)?,
        author: row.get(4)?,
        published: row.get(5)?,
        read: row.get::<_, i64>(6)? != 0,
        starred: row.get::<_, i64>(7)? != 0,
    })
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
        let first = store.upsert_feed(&sub("https://a.com/feed", None)).unwrap();
        let again = store.upsert_feed(&sub("https://a.com/feed", None)).unwrap();
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
        let feed = store.upsert_feed(&sub("https://a.com/feed", None)).unwrap();

        let first = vec![item("a", "One"), item("b", "Two")];
        assert_eq!(store.save_items(feed, &first).unwrap(), 2);

        let second = vec![item("b", "Two"), item("c", "Three")];
        assert_eq!(store.save_items(feed, &second).unwrap(), 1);
        assert_eq!(store.unread_count().unwrap(), 3);
    }

    #[test]
    fn listing_defaults_to_unread_and_reports_the_total() {
        let mut store = Store::open_in_memory().unwrap();
        let feed = store
            .upsert_feed(&sub("https://a.com/feed", Some("Tech")))
            .unwrap();
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
    fn a_feed_supplied_body_is_stored_and_read_back() {
        let mut store = Store::open_in_memory().unwrap();
        let feed = store.upsert_feed(&sub("https://a.com/feed", None)).unwrap();
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
        let feed = store.upsert_feed(&sub("https://a.com/feed", None)).unwrap();
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
        let feed = store.upsert_feed(&sub("https://a.com/feed", None)).unwrap();
        store
            .save_items(
                feed,
                &[item("a", "Async Rust in practice"), item("b", "Go modules")],
            )
            .unwrap();

        let found = store.search("rust", 10).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "a");
    }

    #[test]
    fn the_index_follows_an_edited_title() {
        let mut store = Store::open_in_memory().unwrap();
        let feed = store.upsert_feed(&sub("https://a.com/feed", None)).unwrap();
        store
            .save_items(feed, &[item("a", "Draft heading")])
            .unwrap();
        store
            .save_items(feed, &[item("a", "Published heading")])
            .unwrap();

        assert!(store.search("draft", 10).unwrap().is_empty());
        assert_eq!(store.search("published", 10).unwrap().len(), 1);
    }

    #[test]
    fn a_folder_filter_only_matches_its_own_feeds() {
        let mut store = Store::open_in_memory().unwrap();
        let tech = store
            .upsert_feed(&sub("https://a.com/feed", Some("Tech")))
            .unwrap();
        let news = store
            .upsert_feed(&sub("https://b.com/feed", Some("News")))
            .unwrap();
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
        let feed = store.upsert_feed(&sub("https://a.com/feed", None)).unwrap();
        store.save_items(feed, &[item("a", "One")]).unwrap();

        let ids = vec!["a".to_string()];
        assert_eq!(store.set_flag(&ids, Flag::Read).unwrap(), 1);
        store.set_flag(&ids, Flag::Read).unwrap();
        assert_eq!(store.unread_count().unwrap(), 0);
    }

    #[test]
    fn an_edited_item_keeps_its_read_flag() {
        let mut store = Store::open_in_memory().unwrap();
        let feed = store.upsert_feed(&sub("https://a.com/feed", None)).unwrap();
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
    fn a_reimport_can_add_a_folder_but_not_erase_one() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_feed(&sub("https://a.com/feed", Some("Tech")))
            .unwrap();
        store.upsert_feed(&sub("https://a.com/feed", None)).unwrap();
        assert_eq!(store.feeds().unwrap()[0].folder.as_deref(), Some("Tech"));
    }
}
