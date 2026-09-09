//! Capability-scoped `SQLite` persistence and capture-directory helpers.

use std::{
    cell::RefCell,
    io::{Error as IoError, Result as IoResult},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
#[cfg(unix)]
use cap_std::fs::{Permissions, PermissionsExt as _};
use cap_std::{ambient_authority, fs::Dir};
use directories::ProjectDirs;
use rusqlite::{Connection, OptionalExtension as _, params};

use crate::{MAX_HISTORY_PAGE_SIZE, history::HistoryEntry};

const DATABASE_FILE: &str = "sharer.sqlite3";
const SQLITE_CACHE_KIB: i64 = -512;

/// Minimal persistence interface used by configuration code.
pub trait Storage {
    /// Read a named settings value.
    ///
    /// # Errors
    ///
    /// Returns an error when the database cannot be read.
    fn read(&self, name: &str) -> IoResult<Option<Vec<u8>>>;

    /// Replace a named settings value.
    ///
    /// # Errors
    ///
    /// Returns an error when the value cannot be committed.
    fn write(&self, name: &str, contents: &[u8]) -> IoResult<()>;
}

/// Storage rooted at this application's platform configuration directory.
pub struct PlatformStorage {
    connection: RefCell<Connection>,
}

impl PlatformStorage {
    /// Open the application's platform configuration database.
    ///
    /// # Errors
    ///
    /// Returns an error when no configuration directory is available or `SQLite` cannot open it.
    pub fn open() -> Result<Self> {
        let directories =
            ProjectDirs::from("re", "", "sharer").context("configuration directory unavailable")?;

        Self::open_in(directories.config_dir())
    }

    /// Open a `ShareR` database in a specific directory.
    ///
    /// This is primarily useful for isolated tests and portable installations.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory or `SQLite` database cannot be initialized.
    pub fn open_in(path: &Path) -> Result<Self> {
        let authority = ambient_authority();

        Dir::create_ambient_dir_all(path, authority)
            .with_context(|| format!("failed to create {}", path.display()))?;
        #[cfg(unix)]
        let directory = Dir::open_ambient_dir(path, authority)
            .with_context(|| format!("failed to open {}", path.display()))?;
        let connection = Connection::open(path.join(DATABASE_FILE))
            .context("failed to open the ShareR database")?;

        connection
            .pragma_update(None, "cache_size", SQLITE_CACHE_KIB)
            .context("failed to limit the SQLite page cache")?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE IF NOT EXISTS settings (
                     key TEXT PRIMARY KEY NOT NULL,
                     value BLOB NOT NULL
                 ) WITHOUT ROWID;
                 CREATE TABLE IF NOT EXISTS upload_history (
                     id INTEGER PRIMARY KEY,
                     original_name TEXT NOT NULL,
                     size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
                     link TEXT NOT NULL,
                     delete_url TEXT NOT NULL,
                     expires_at TEXT NOT NULL
                 );",
            )
            .context("failed to initialize the ShareR database")?;

        #[cfg(unix)]
        directory
            .set_permissions(DATABASE_FILE, Permissions::from_mode(0o600))
            .context("failed to make the ShareR database private")?;

        Ok(Self {
            connection: RefCell::new(connection),
        })
    }

    /// Insert one upload without retaining previous entries in memory.
    ///
    /// # Errors
    ///
    /// Returns an error when the entry cannot be represented or committed.
    pub fn insert_history(&self, entry: &HistoryEntry) -> Result<()> {
        let size_bytes = i64::try_from(entry.size_bytes)
            .context("upload size exceeds SQLite's integer range")?;

        self.connection
            .borrow()
            .execute(
                "INSERT INTO upload_history
                 (original_name, size_bytes, link, delete_url, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    entry.filename,
                    size_bytes,
                    entry.link,
                    entry.delete_url,
                    entry.expires_at
                ],
            )
            .context("failed to append upload history")?;

        Ok(())
    }

    /// Load one newest-first page from the complete upload archive.
    ///
    /// # Errors
    ///
    /// Returns an error when the page bounds overflow or `SQLite` cannot read a row.
    pub fn history_page(&self, offset: usize, limit: usize) -> Result<Vec<HistoryEntry>> {
        anyhow::ensure!(
            (1..=MAX_HISTORY_PAGE_SIZE).contains(&limit),
            "history page size must be between 1 and {MAX_HISTORY_PAGE_SIZE}"
        );
        let offset = i64::try_from(offset).context("history offset is too large")?;
        let limit = i64::try_from(limit).context("history page size is too large")?;
        let connection = self.connection.borrow();
        let mut statement = connection
            .prepare(
                "SELECT original_name, size_bytes, link, delete_url, expires_at
                 FROM upload_history
                 ORDER BY id DESC
                 LIMIT ?1 OFFSET ?2",
            )
            .context("failed to prepare history page query")?;
        let rows = statement
            .query_map(params![limit, offset], |row| {
                Ok(HistoryEntry {
                    filename: row.get(0)?,
                    size_bytes: row.get(1)?,
                    link: row.get(2)?,
                    delete_url: row.get(3)?,
                    expires_at: row.get(4)?,
                })
            })
            .context("failed to query upload history")?;
        let mut entries = Vec::with_capacity(usize::try_from(limit).unwrap_or_default());

        for row in rows {
            entries.push(row.context("failed to decode upload history")?);
        }

        Ok(entries)
    }

    /// Count every upload retained in the database.
    ///
    /// # Errors
    ///
    /// Returns an error when `SQLite` cannot count the archive.
    pub fn history_count(&self) -> Result<u64> {
        let count = self
            .connection
            .borrow()
            .query_row("SELECT COUNT(*) FROM upload_history", [], |row| {
                row.get::<_, i64>(0)
            })
            .context("failed to count upload history")?;

        u64::try_from(count).context("SQLite returned a negative history count")
    }

    fn read_setting(&self, name: &str) -> rusqlite::Result<Option<Vec<u8>>> {
        self.connection
            .borrow()
            .query_row("SELECT value FROM settings WHERE key = ?1", [name], |row| {
                row.get(0)
            })
            .optional()
    }

    fn write_setting(&self, name: &str, contents: &[u8]) -> rusqlite::Result<()> {
        self.connection.borrow().execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![name, contents],
        )?;

        Ok(())
    }
}

impl Storage for PlatformStorage {
    fn read(&self, name: &str) -> IoResult<Option<Vec<u8>>> {
        self.read_setting(name).map_err(sqlite_io_error)
    }

    fn write(&self, name: &str, contents: &[u8]) -> IoResult<()> {
        self.write_setting(name, contents).map_err(sqlite_io_error)
    }
}

impl std::fmt::Debug for PlatformStorage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PlatformStorage")
            .finish_non_exhaustive()
    }
}

fn sqlite_io_error(error: rusqlite::Error) -> IoError {
    IoError::other(error)
}

/// Return the default directory beside the database for retained captures.
///
/// # Errors
///
/// Returns an error when the platform configuration directory is unavailable.
pub fn default_capture_directory() -> Result<PathBuf> {
    ProjectDirs::from("re", "", "sharer")
        .map(|directories| directories.config_dir().join("files"))
        .context("capture directory unavailable")
}

/// Create the configured local capture directory when it does not exist.
///
/// # Errors
///
/// Returns an error when the directory cannot be created.
pub fn ensure_capture_directory(path: &Path) -> Result<()> {
    Dir::create_ambient_dir_all(path, ambient_authority())
        .with_context(|| format!("failed to create {}", path.display()))
}
