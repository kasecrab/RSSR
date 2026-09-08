use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

use crate::opml::Subscription;
use crate::{Error, Result};

const SCHEMA: &str = r#"
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

#[derive(Debug, Clone)]
pub struct Feed {
    pub id: i64,
    pub url: String,
    pub title: Option<String>,
    pub folder: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
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
        if version == 0 {
            self.conn.execute_batch(SCHEMA)?;
            self.conn.execute_batch("PRAGMA user_version = 1")?;
        }
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

    pub fn default_path() -> Result<PathBuf> {
        let dir = dirs::data_dir()
            .ok_or_else(|| Error::Config("no data directory for this platform".into()))?;
        Ok(dir.join("rssr").join("rssr.db"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
