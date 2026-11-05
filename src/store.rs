//! Clipboard history in SQLite. Text lives in the database; images are files
//! under `images/` (original bytes plus a small PNG thumbnail).

use anyhow::{Context, bail};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Longest side of generated thumbnails, in pixels.
const THUMB_SIZE: u32 = 256;
/// Characters of text loaded for list rows and search.
const SNIPPET_CHARS: i64 = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text = 0,
    Image = 1,
}

/// Lightweight entry for the list view (text is truncated to a snippet).
#[derive(Debug, Clone)]
pub struct Entry {
    pub id: i64,
    pub kind: Kind,
    pub mime: String,
    /// Unix milliseconds of the most recent copy.
    pub copied_at: i64,
    pub size: i64,
    pub snippet: String,
    pub width: u32,
    pub height: u32,
    pub hash: String,
}

pub struct Store {
    conn: Connection,
    images: PathBuf,
}

impl Store {
    /// Opens the history in the default data directory.
    pub fn open() -> anyhow::Result<Self> {
        Self::open_at(&crate::data_dir())
    }

    pub fn open_at(dir: &Path) -> anyhow::Result<Self> {
        let images = dir.join("images");
        std::fs::create_dir_all(&images).with_context(|| format!("creating {}", images.display()))?;
        // Clipboard history is private: only the owner may read the directory.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        let conn = Connection::open(dir.join("history.db"))?;
        conn.busy_timeout(Duration::from_secs(3))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS entries (
                 id        INTEGER PRIMARY KEY,
                 hash      TEXT    NOT NULL UNIQUE,
                 kind      INTEGER NOT NULL,
                 mime      TEXT    NOT NULL,
                 copied_at INTEGER NOT NULL,
                 size      INTEGER NOT NULL,
                 text      TEXT,
                 width     INTEGER NOT NULL DEFAULT 0,
                 height    INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS entries_by_time ON entries (copied_at DESC);",
        )?;
        Ok(Self { conn, images })
    }

    pub fn image_path(&self, hash: &str) -> PathBuf {
        self.images.join(hash)
    }

    pub fn thumb_path(&self, hash: &str) -> PathBuf {
        self.images.join(format!("{hash}.thumb.png"))
    }

    /// Records clipboard contents. Copying something already in the history
    /// only moves it to the top. Returns the entry id.
    pub fn add(&self, kind: Kind, mime: &str, data: &[u8], at: i64, max_entries: u32) -> anyhow::Result<i64> {
        let hash = blake3::hash(data).to_hex().to_string();
        if let Some(id) = self.touch_hash(&hash, at)? {
            return Ok(id);
        }

        let (text, width, height) = match kind {
            Kind::Text => {
                let text = std::str::from_utf8(data).context("clipboard text is not UTF-8")?;
                if text.trim().is_empty() {
                    bail!("clipboard text is empty");
                }
                (Some(text), 0, 0)
            }
            Kind::Image => {
                let img = image::load_from_memory(data).context("decoding image")?;
                img.thumbnail(THUMB_SIZE, THUMB_SIZE)
                    .save_with_format(self.thumb_path(&hash), image::ImageFormat::Png)?;
                std::fs::write(self.image_path(&hash), data)?;
                (None, img.width(), img.height())
            }
        };

        self.conn.execute(
            "INSERT INTO entries (hash, kind, mime, copied_at, size, text, width, height)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                hash,
                kind as i64,
                mime,
                at,
                data.len() as i64,
                text,
                width,
                height
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        self.prune(max_entries)?;
        Ok(id)
    }

    fn touch_hash(&self, hash: &str, at: i64) -> anyhow::Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "UPDATE entries SET copied_at = ?1 WHERE hash = ?2 RETURNING id",
                params![at, hash],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Moves an entry to the top of the history.
    pub fn touch(&self, id: i64, at: i64) -> anyhow::Result<()> {
        self.conn
            .execute("UPDATE entries SET copied_at = ?1 WHERE id = ?2", params![at, id])?;
        Ok(())
    }

    /// All entries, newest first.
    pub fn list(&self) -> anyhow::Result<Vec<Entry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, mime, copied_at, size, coalesce(substr(text, 1, ?1), ''), width, height, hash
             FROM entries ORDER BY copied_at DESC, id DESC",
        )?;
        let rows = stmt.query_map([SNIPPET_CHARS], |row| {
            Ok(Entry {
                id: row.get(0)?,
                kind: if row.get::<_, i64>(1)? == Kind::Image as i64 {
                    Kind::Image
                } else {
                    Kind::Text
                },
                mime: row.get(2)?,
                copied_at: row.get(3)?,
                size: row.get(4)?,
                snippet: row.get(5)?,
                width: row.get(6)?,
                height: row.get(7)?,
                hash: row.get(8)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Full text of a text entry.
    pub fn text(&self, id: i64) -> anyhow::Result<String> {
        Ok(self.conn.query_row(
            "SELECT coalesce(text, '') FROM entries WHERE id = ?1",
            [id],
            |row| row.get(0),
        )?)
    }

    /// Kind, MIME type and raw bytes of an entry, for putting it back on the clipboard.
    pub fn content(&self, id: i64) -> anyhow::Result<(Kind, String, Vec<u8>)> {
        let (kind, mime, hash, text): (i64, String, String, Option<String>) = self.conn.query_row(
            "SELECT kind, mime, hash, text FROM entries WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        if kind == Kind::Image as i64 {
            Ok((Kind::Image, mime, std::fs::read(self.image_path(&hash))?))
        } else {
            Ok((Kind::Text, mime, text.unwrap_or_default().into_bytes()))
        }
    }

    /// Id of the newest entry.
    pub fn latest(&self) -> anyhow::Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM entries ORDER BY copied_at DESC, id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn delete(&self, id: i64) -> anyhow::Result<()> {
        let hash: Option<(i64, String)> = self
            .conn
            .query_row(
                "DELETE FROM entries WHERE id = ?1 RETURNING kind, hash",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((kind, hash)) = hash {
            self.remove_files(kind, &hash);
        }
        Ok(())
    }

    pub fn clear(&self) -> anyhow::Result<()> {
        self.conn.execute("DELETE FROM entries", [])?;
        self.conn.execute_batch("VACUUM")?;
        for file in std::fs::read_dir(&self.images)?.flatten() {
            let _ = std::fs::remove_file(file.path());
        }
        Ok(())
    }

    /// Deletes the oldest entries beyond `max_entries`.
    pub fn prune(&self, max_entries: u32) -> anyhow::Result<()> {
        let mut stmt = self.conn.prepare(
            "DELETE FROM entries WHERE id IN (
                 SELECT id FROM entries ORDER BY copied_at DESC, id DESC LIMIT -1 OFFSET ?1
             ) RETURNING kind, hash",
        )?;
        let removed = stmt
            .query_map([max_entries], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for (kind, hash) in removed {
            self.remove_files(kind, &hash);
        }
        Ok(())
    }

    fn remove_files(&self, kind: i64, hash: &str) {
        if kind == Kind::Image as i64 {
            let _ = std::fs::remove_file(self.image_path(hash));
            let _ = std::fs::remove_file(self.thumb_path(hash));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(name: &str) -> (Store, PathBuf) {
        let dir = std::env::temp_dir().join(format!("wisp-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (Store::open_at(&dir).unwrap(), dir)
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        image::RgbaImage::new(width, height)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    #[test]
    fn duplicate_moves_to_top() {
        let (store, dir) = temp_store("dedupe");
        let a = store.add(Kind::Text, "text/plain", b"alpha", 1, 10).unwrap();
        store.add(Kind::Text, "text/plain", b"beta", 2, 10).unwrap();
        let again = store.add(Kind::Text, "text/plain", b"alpha", 3, 10).unwrap();
        assert_eq!(a, again);
        let list = store.list().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].snippet, "alpha");
        assert_eq!(list[0].copied_at, 3);
        assert_eq!(store.latest().unwrap(), Some(a));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn prune_keeps_newest_and_removes_files() {
        let (store, dir) = temp_store("prune");
        let img = store.add(Kind::Image, "image/png", &png(600, 300), 1, 2).unwrap();
        let hash = store.list().unwrap()[0].hash.clone();
        assert!(store.thumb_path(&hash).exists());
        store.add(Kind::Text, "text/plain", b"two", 2, 2).unwrap();
        store.add(Kind::Text, "text/plain", b"three", 3, 2).unwrap();
        let list = store.list().unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.iter().all(|e| e.id != img));
        assert!(!store.image_path(&hash).exists());
        assert!(!store.thumb_path(&hash).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn image_roundtrip_and_delete() {
        let (store, dir) = temp_store("image");
        let data = png(40, 20);
        let id = store.add(Kind::Image, "image/png", &data, 1, 10).unwrap();
        let entry = &store.list().unwrap()[0];
        assert_eq!((entry.width, entry.height), (40, 20));
        let (kind, mime, bytes) = store.content(id).unwrap();
        assert_eq!((kind, mime.as_str(), bytes), (Kind::Image, "image/png", data));
        store.delete(id).unwrap();
        assert!(store.list().unwrap().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_blank_text() {
        let (store, dir) = temp_store("blank");
        assert!(store.add(Kind::Text, "text/plain", b"  \n", 1, 10).is_err());
        assert!(store.list().unwrap().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
