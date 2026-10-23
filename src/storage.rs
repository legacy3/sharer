//! Capability-scoped `SQLite` persistence and capture-directory helpers.

use std::{
    cell::RefCell,
    io::{Error as IoError, Result as IoResult},
    path::{Path, PathBuf},
    time::{Duration, UNIX_EPOCH},
};

use anyhow::{Context, Result};
#[cfg(unix)]
use cap_std::fs::{Permissions, PermissionsExt as _};
use cap_std::{ambient_authority, fs::Dir};
use directories::{ProjectDirs, UserDirs};
use rusqlite::{Connection, OptionalExtension as _, params, types::Value};

use crate::{
    MAX_HISTORY_PAGE_SIZE,
    history::{HistoryEntry, HistoryFilter, HistorySort, HistorySortColumn, SortDirection},
};

const DATABASE_FILE: &str = "sharer.sqlite3";
const SQLITE_CACHE_KIB: i64 = -512;
const HISTORY_COLUMNS: &str =
    "id, created_at, original_name, size_bytes, link, delete_url, expires_at, local_path";
// Search only ordinary display values. Deletion URLs are private capabilities.
const HISTORY_SELECTION: &str = "WHERE (?1 = ''
    OR instr(lower(original_name), lower(?1)) > 0
    OR instr(lower(link), lower(?1)) > 0
    OR instr(lower(local_path), lower(?1)) > 0)
    AND (?2 = 0 OR (?2 = 1 AND link = '') OR (?2 = 2 AND link <> ''))";

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
    directory: PathBuf,
}

/// Outcome of checking the local paths on one bounded history page.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HistoryLocalPathReconciliation {
    pub cleared: usize,
    pub unavailable: usize,
    pub not_regular: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LocalPathState {
    RegularFile,
    Missing,
    Unavailable,
    NotRegular,
}

/// Resolve the directory that holds settings, history, and the instance lock.
///
/// # Errors
///
/// Returns an error when no configuration directory is available.
pub fn config_directory() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("SHARER_CONFIG_DIR").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }

    ProjectDirs::from("re", "", "sharer")
        .map(|directories| directories.config_dir().to_owned())
        .context("configuration directory unavailable")
}

impl PlatformStorage {
    /// Open the application's platform configuration database.
    ///
    /// Set `SHARER_CONFIG_DIR` to use a portable profile in a specific directory.
    ///
    /// # Errors
    ///
    /// Returns an error when no configuration directory is available or `SQLite` cannot open it.
    pub fn open() -> Result<Self> {
        Self::open_in(&config_directory()?)
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
        let profile_directory =
            std::path::absolute(path).context("failed to resolve the ShareR profile directory")?;

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
                     created_at INTEGER NOT NULL DEFAULT 0,
                     original_name TEXT NOT NULL,
                     size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
                     link TEXT NOT NULL,
                     delete_url TEXT NOT NULL,
                     expires_at TEXT NOT NULL,
                     local_path TEXT NOT NULL DEFAULT ''
                 );",
            )
            .context("failed to initialize the ShareR database")?;
        let history_columns = {
            let mut statement = connection
                .prepare("PRAGMA table_info(upload_history)")
                .context("failed to inspect upload history schema")?;
            let columns = statement
                .query_map([], |row| row.get::<_, String>(1))
                .context("failed to read upload history schema")?;
            let mut names = Vec::new();

            for column in columns {
                names.push(column.context("failed to decode upload history schema")?);
            }

            names
        };

        if !history_columns.iter().any(|column| column == "local_path") {
            connection
                .execute(
                    "ALTER TABLE upload_history ADD COLUMN local_path TEXT NOT NULL DEFAULT ''",
                    [],
                )
                .context("failed to migrate upload history for local captures")?;
        }

        if !history_columns.iter().any(|column| column == "created_at") {
            connection
                .execute(
                    "ALTER TABLE upload_history ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0",
                    [],
                )
                .context("failed to migrate upload history creation dates")?;
        }

        backfill_history_creation_dates(&connection)?;
        connection
            .execute_batch(
                "CREATE INDEX IF NOT EXISTS history_filename
                     ON upload_history (original_name COLLATE NOCASE, id);
                 CREATE INDEX IF NOT EXISTS history_created_at
                     ON upload_history (created_at, id);
                 CREATE INDEX IF NOT EXISTS history_size
                     ON upload_history (size_bytes, id);
                 CREATE INDEX IF NOT EXISTS history_expiration_ascending
                     ON upload_history ((expires_at = '') ASC, expires_at ASC, id ASC);
                 CREATE INDEX IF NOT EXISTS history_expiration_descending
                     ON upload_history ((expires_at = '') ASC, expires_at DESC, id DESC);",
            )
            .context("failed to index capture history")?;

        #[cfg(unix)]
        directory
            .set_permissions(DATABASE_FILE, Permissions::from_mode(0o600))
            .context("failed to make the ShareR database private")?;

        Ok(Self {
            connection: RefCell::new(connection),
            directory: profile_directory,
        })
    }

    /// Return the resolved profile directory for an independent background connection.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Limit this connection's wait for a conflicting database lock.
    ///
    /// # Errors
    ///
    /// Returns an error when `SQLite` cannot install the timeout.
    pub fn set_lock_wait_timeout(&self, timeout: Duration) -> Result<()> {
        self.connection
            .borrow()
            .busy_timeout(timeout)
            .context("failed to configure the database lock timeout")
    }

    /// Insert one upload without retaining previous entries in memory.
    ///
    /// # Errors
    ///
    /// Returns an error when the entry cannot be represented or committed.
    pub fn insert_history(&self, entry: &HistoryEntry) -> Result<i64> {
        let size_bytes = i64::try_from(entry.size_bytes)
            .context("upload size exceeds SQLite's integer range")?;

        self.connection
            .borrow()
            .execute(
                "INSERT INTO upload_history
                 (created_at, original_name, size_bytes, link, delete_url, expires_at, local_path)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    entry.created_at,
                    entry.filename,
                    size_bytes,
                    entry.link,
                    entry.delete_url,
                    entry.expires_at,
                    entry.local_path
                ],
            )
            .context("failed to append upload history")?;

        Ok(self.connection.borrow().last_insert_rowid())
    }

    /// Read a history record before starting an action on its local file.
    ///
    /// # Errors
    ///
    /// Returns an error when `SQLite` cannot read or decode the record.
    pub fn history_entry(&self, id: i64) -> Result<Option<HistoryEntry>> {
        self.connection
            .borrow()
            .query_row(
                "SELECT id, created_at, original_name, size_bytes, link, delete_url, expires_at, local_path
                 FROM upload_history WHERE id = ?1",
                [id],
                history_entry_from_row,
            )
            .optional()
            .context("failed to read history entry")
    }

    /// Attach an upload receipt to an unchanged local-only record.
    ///
    /// Preserve a new receipt if its original record was changed or removed.
    /// This avoids overwriting another row or losing returned deletion links.
    /// Files on disk are never changed.
    ///
    /// # Errors
    ///
    /// Returns an error for an ineligible original, an invalid receipt, or a database failure.
    pub fn complete_history_upload(
        &self,
        original: &HistoryEntry,
        uploaded: &mut HistoryEntry,
    ) -> Result<()> {
        anyhow::ensure!(
            original.link.is_empty() && !original.local_path.is_empty(),
            "only local-only history entries can be uploaded"
        );
        anyhow::ensure!(
            !uploaded.link.is_empty() && uploaded.local_path == original.local_path,
            "history upload receipt must retain the original local path"
        );
        let original_size = i64::try_from(original.size_bytes)
            .context("original size exceeds SQLite's integer range")?;
        let uploaded_size = i64::try_from(uploaded.size_bytes)
            .context("upload size exceeds SQLite's integer range")?;
        let updated = self
            .connection
            .borrow()
            .execute(
                "UPDATE upload_history
             SET original_name = ?1, size_bytes = ?2, link = ?3, delete_url = ?4, expires_at = ?5
             WHERE id = ?6 AND created_at = ?7 AND original_name = ?8 AND size_bytes = ?9
               AND link = '' AND delete_url = ?10 AND expires_at = ?11 AND local_path = ?12",
                params![
                    uploaded.filename,
                    uploaded_size,
                    uploaded.link,
                    uploaded.delete_url,
                    uploaded.expires_at,
                    original.id,
                    original.created_at,
                    original.filename,
                    original_size,
                    original.delete_url,
                    original.expires_at,
                    original.local_path,
                ],
            )
            .context("failed to update local capture history")?;

        if updated == 0 {
            uploaded.id = self.insert_history(uploaded)?;
        } else {
            uploaded.id = original.id;
            uploaded.created_at = original.created_at;
        }

        Ok(())
    }

    /// Load one newest-first page from the complete upload archive.
    ///
    /// # Errors
    ///
    /// Returns an error when the page bounds overflow or `SQLite` cannot read a row.
    pub fn history_page(&self, offset: usize, limit: usize) -> Result<Vec<HistoryEntry>> {
        self.history_page_sorted(offset, limit, HistorySort::default())
    }

    /// Load one page from the complete capture archive in the requested table order.
    ///
    /// # Errors
    ///
    /// Returns an error when the page bounds overflow or `SQLite` cannot read a row.
    pub fn history_page_sorted(
        &self,
        offset: usize,
        limit: usize,
        sort: HistorySort,
    ) -> Result<Vec<HistoryEntry>> {
        self.history_page_filtered(offset, limit, sort, "", HistoryFilter::All)
    }

    /// Load one bounded, sorted page matching a literal substring and receipt state.
    ///
    /// Search matches filenames, public links, and local paths with `SQLite`'s ASCII case folding.
    /// Whitespace around the query is ignored, and deletion URLs are never searched.
    /// Local-only records include missing files so their history can still be found and removed.
    ///
    /// # Errors
    ///
    /// Returns an error when page bounds overflow or `SQLite` cannot read a row.
    pub fn history_page_filtered(
        &self,
        offset: usize,
        limit: usize,
        sort: HistorySort,
        query: &str,
        filter: HistoryFilter,
    ) -> Result<Vec<HistoryEntry>> {
        let offset = i64::try_from(offset).context("history offset is too large")?;
        let page_limit = history_page_limit(limit)?;
        let order = history_order(sort);
        let sql = format!(
            "SELECT {HISTORY_COLUMNS}
             FROM upload_history {HISTORY_SELECTION} ORDER BY {order} LIMIT ?3 OFFSET ?4"
        );
        let connection = self.connection.borrow();

        read_history_page(
            &connection,
            &sql,
            params![query.trim(), filter.index(), page_limit, offset],
            limit,
        )
    }

    /// Continue a matching history page strictly after its last sorted record.
    ///
    /// The cursor retains the supplied record's sort value and ID even after clearing or deletion.
    /// Search and filter semantics match [`Self::history_page_filtered`].
    /// Records without an expiration remain last in either order.
    ///
    /// # Errors
    ///
    /// Returns an error when the page limit or cursor cannot be represented or `SQLite` fails.
    pub fn history_page_filtered_after(
        &self,
        limit: usize,
        sort: HistorySort,
        query: &str,
        filter: HistoryFilter,
        after: &HistoryEntry,
    ) -> Result<Vec<HistoryEntry>> {
        let page_limit = history_page_limit(limit)?;
        let value = history_cursor_value(sort.column, after)?;
        let (ties, following) = history_cursor_predicates(sort);
        let order = match (sort.column, sort.direction) {
            (HistorySortColumn::ExpiresAt, SortDirection::Ascending) => "expires_at ASC, id ASC",
            (HistorySortColumn::ExpiresAt, SortDirection::Descending) => "expires_at DESC, id DESC",
            _ => history_order(sort),
        };
        let sql = format!(
            "SELECT {HISTORY_COLUMNS} FROM upload_history
             {HISTORY_SELECTION} AND {ties} ORDER BY {order} LIMIT ?3"
        );
        let connection = self.connection.borrow();
        let mut entries = read_history_page(
            &connection,
            &sql,
            params![query.trim(), filter.index(), page_limit, value, after.id],
            limit,
        )?;

        // Separate equal-key rows from later keys so SQLite seeks directly to the cursor ID.
        // A tuple comparison can still walk every earlier tie, even with a composite index.
        if entries.len() < limit
            && (sort.column != HistorySortColumn::ExpiresAt || !after.expires_at.is_empty())
        {
            let remaining = limit - entries.len();
            let sql = format!(
                "SELECT {HISTORY_COLUMNS} FROM upload_history
                 {HISTORY_SELECTION} AND {following} ORDER BY {order} LIMIT ?3"
            );

            entries.extend(read_history_page(
                &connection,
                &sql,
                params![
                    query.trim(),
                    filter.index(),
                    history_page_limit(remaining)?,
                    value
                ],
                remaining,
            )?);
        }

        // Keep expiry groups separate so both directions seek their index without sorting.
        if sort.column == HistorySortColumn::ExpiresAt
            && !after.expires_at.is_empty()
            && entries.len() < limit
        {
            let remaining = limit - entries.len();
            let sql = format!(
                "SELECT {HISTORY_COLUMNS} FROM upload_history {HISTORY_SELECTION}
                 AND (expires_at = '') = 1 ORDER BY {order} LIMIT ?3"
            );

            entries.extend(read_history_page(
                &connection,
                &sql,
                params![query.trim(), filter.index(), history_page_limit(remaining)?],
                remaining,
            )?);
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

    /// Count records matching the same public-field search and filter as a page.
    ///
    /// # Errors
    ///
    /// Returns an error when `SQLite` cannot count the matching records.
    pub fn history_count_filtered(&self, query: &str, filter: HistoryFilter) -> Result<u64> {
        if query.trim().is_empty() && filter == HistoryFilter::All {
            return self.history_count();
        }

        let sql = format!("SELECT COUNT(*) FROM upload_history {HISTORY_SELECTION}");
        let count = self
            .connection
            .borrow()
            .query_row(&sql, params![query.trim(), filter.index()], |row| {
                row.get::<_, i64>(0)
            })
            .context("failed to count upload history")?;

        u64::try_from(count).context("SQLite returned a negative history count")
    }

    /// Remove one history record without deleting local or remote files.
    ///
    /// # Errors
    ///
    /// Returns an error when the record cannot be removed.
    pub fn remove_history(&self, id: i64) -> Result<()> {
        self.connection
            .borrow()
            .execute("DELETE FROM upload_history WHERE id = ?1", [id])
            .context("failed to remove history entry")?;

        Ok(())
    }

    /// Clear the saved local path after its file has been deleted.
    ///
    /// # Errors
    ///
    /// Returns an error when the history record cannot be updated.
    pub fn clear_history_local_path(&self, id: i64) -> Result<()> {
        self.connection
            .borrow()
            .execute(
                "UPDATE upload_history SET local_path = '' WHERE id = ?1",
                [id],
            )
            .context("failed to update history entry")?;

        Ok(())
    }

    /// Clear local actions for loaded history rows whose files no longer exist.
    ///
    /// Only the supplied, already bounded page is inspected.
    /// Remote links and the history records themselves are preserved.
    ///
    /// # Errors
    ///
    /// Returns an error when the stale local paths cannot be cleared atomically.
    pub fn reconcile_missing_history_local_paths(
        &self,
        entries: &mut [HistoryEntry],
    ) -> Result<HistoryLocalPathReconciliation> {
        self.reconcile_missing_history_local_paths_cancellable(entries, || false)
    }

    /// Reconcile missing paths while allowing cancellation between filesystem checks and writes.
    ///
    /// Cancellation before commit rolls back all pending changes and preserves the supplied paths.
    /// After a successful commit, every cleared path is reflected in the supplied records.
    ///
    /// # Errors
    ///
    /// Returns an error when the stale local paths cannot be cleared atomically.
    pub fn reconcile_missing_history_local_paths_cancellable(
        &self,
        entries: &mut [HistoryEntry],
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<HistoryLocalPathReconciliation> {
        let mut report = HistoryLocalPathReconciliation::default();
        let mut missing = Vec::new();

        for entry in entries.iter().filter(|entry| !entry.local_path.is_empty()) {
            if cancelled() {
                return Ok(report);
            }

            match classify_local_path(Path::new(&entry.local_path)) {
                LocalPathState::RegularFile => {}
                LocalPathState::Missing => missing.push((entry.id, entry.local_path.clone())),
                LocalPathState::Unavailable => report.unavailable += 1,
                LocalPathState::NotRegular => report.not_regular += 1,
            }
        }

        if missing.is_empty() || cancelled() {
            return Ok(report);
        }

        let mut connection = self.connection.borrow_mut();
        let transaction = connection
            .transaction()
            .context("failed to start local history reconciliation")?;
        let mut reconciled = Vec::with_capacity(missing.len());

        for (id, path) in &missing {
            if cancelled() {
                return Ok(report);
            }

            let changed = transaction
                .execute(
                    "UPDATE upload_history SET local_path = ''
                     WHERE id = ?1 AND local_path = ?2",
                    params![id, path],
                )
                .context("failed to reconcile a missing local capture")?;

            if changed != 0 {
                reconciled.push((*id, path.as_str()));
            }
        }

        if cancelled() {
            return Ok(report);
        }

        transaction
            .commit()
            .context("failed to commit local history reconciliation")?;

        for entry in entries {
            if reconciled
                .iter()
                .any(|(id, path)| *id == entry.id && *path == entry.local_path)
            {
                entry.local_path.clear();
            }
        }

        report.cleared = reconciled.len();

        Ok(report)
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

fn history_page_limit(limit: usize) -> Result<i64> {
    anyhow::ensure!(
        (1..=MAX_HISTORY_PAGE_SIZE).contains(&limit),
        "history page size must be between 1 and {MAX_HISTORY_PAGE_SIZE}"
    );

    i64::try_from(limit).context("history page size is too large")
}

fn read_history_page(
    connection: &Connection,
    sql: &str,
    parameters: impl rusqlite::Params,
    limit: usize,
) -> Result<Vec<HistoryEntry>> {
    let mut statement = connection
        .prepare(sql)
        .context("failed to prepare history page query")?;
    let rows = statement
        .query_map(parameters, history_entry_from_row)
        .context("failed to query upload history")?;
    let mut entries = Vec::with_capacity(limit);

    for row in rows {
        entries.push(row.context("failed to decode upload history")?);
    }

    Ok(entries)
}

fn history_cursor_value(column: HistorySortColumn, after: &HistoryEntry) -> Result<Value> {
    Ok(match column {
        HistorySortColumn::Filename => Value::Text(after.filename.clone()),

        HistorySortColumn::CreatedAt => Value::Integer(after.created_at),

        HistorySortColumn::Size => Value::Integer(
            i64::try_from(after.size_bytes).context("history cursor size is too large")?,
        ),

        HistorySortColumn::ExpiresAt => Value::Text(after.expires_at.clone()),
    })
}

fn history_cursor_predicates(sort: HistorySort) -> (&'static str, &'static str) {
    match (sort.column, sort.direction) {
        (HistorySortColumn::Filename, SortDirection::Ascending) => (
            "original_name = ?4 COLLATE NOCASE AND id > ?5",
            "original_name > ?4 COLLATE NOCASE",
        ),

        (HistorySortColumn::Filename, SortDirection::Descending) => (
            "original_name = ?4 COLLATE NOCASE AND id < ?5",
            "original_name < ?4 COLLATE NOCASE",
        ),

        (HistorySortColumn::CreatedAt, SortDirection::Ascending) => {
            ("created_at = ?4 AND id > ?5", "created_at > ?4")
        }

        (HistorySortColumn::CreatedAt, SortDirection::Descending) => {
            ("created_at = ?4 AND id < ?5", "created_at < ?4")
        }

        (HistorySortColumn::Size, SortDirection::Ascending) => {
            ("size_bytes = ?4 AND id > ?5", "size_bytes > ?4")
        }

        (HistorySortColumn::Size, SortDirection::Descending) => {
            ("size_bytes = ?4 AND id < ?5", "size_bytes < ?4")
        }

        (HistorySortColumn::ExpiresAt, SortDirection::Ascending) => (
            "(expires_at = '') = (?4 = '') AND expires_at = ?4 AND id > ?5",
            "(expires_at = '') = (?4 = '') AND expires_at > ?4",
        ),

        (HistorySortColumn::ExpiresAt, SortDirection::Descending) => (
            "(expires_at = '') = (?4 = '') AND expires_at = ?4 AND id < ?5",
            "(expires_at = '') = (?4 = '') AND expires_at < ?4",
        ),
    }
}

fn history_order(sort: HistorySort) -> &'static str {
    match (sort.column, sort.direction) {
        (HistorySortColumn::Filename, SortDirection::Ascending) => {
            "original_name COLLATE NOCASE ASC, id ASC"
        }

        (HistorySortColumn::Filename, SortDirection::Descending) => {
            "original_name COLLATE NOCASE DESC, id DESC"
        }

        (HistorySortColumn::CreatedAt, SortDirection::Ascending) => "created_at ASC, id ASC",

        (HistorySortColumn::CreatedAt, SortDirection::Descending) => "created_at DESC, id DESC",

        (HistorySortColumn::Size, SortDirection::Ascending) => "size_bytes ASC, id ASC",

        (HistorySortColumn::Size, SortDirection::Descending) => "size_bytes DESC, id DESC",

        (HistorySortColumn::ExpiresAt, SortDirection::Ascending) => {
            "(expires_at = '') ASC, expires_at ASC, id ASC"
        }

        (HistorySortColumn::ExpiresAt, SortDirection::Descending) => {
            "(expires_at = '') ASC, expires_at DESC, id DESC"
        }
    }
}

fn history_entry_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryEntry> {
    Ok(HistoryEntry {
        id: row.get(0)?,
        created_at: row.get(1)?,
        filename: row.get(2)?,
        size_bytes: row.get(3)?,
        link: row.get(4)?,
        delete_url: row.get(5)?,
        expires_at: row.get(6)?,
        local_path: row.get(7)?,
    })
}

fn classify_local_path(path: &Path) -> LocalPathState {
    classify_local_path_metadata(path_metadata(path).map(|metadata| metadata.is_file()))
}

fn classify_local_path_metadata(metadata: IoResult<bool>) -> LocalPathState {
    match metadata {
        Ok(true) => LocalPathState::RegularFile,
        Ok(_) => LocalPathState::NotRegular,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => LocalPathState::Missing,
        Err(_) => LocalPathState::Unavailable,
    }
}

fn path_metadata(path: &Path) -> IoResult<cap_std::fs::Metadata> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let filename = path
        .file_name()
        .ok_or_else(|| IoError::new(std::io::ErrorKind::InvalidInput, "path has no filename"))?;
    let directory = Dir::open_ambient_dir(parent, ambient_authority())?;

    directory.metadata(filename)
}

fn backfill_history_creation_dates(connection: &Connection) -> Result<()> {
    let candidates = {
        let mut statement = connection
            .prepare(
                "SELECT id, local_path FROM upload_history
                 WHERE created_at = 0 AND local_path <> ''",
            )
            .context("failed to prepare history date migration")?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .context("failed to read legacy local capture paths")?;
        let mut candidates = Vec::new();

        for row in rows {
            candidates.push(row.context("failed to decode a legacy local capture path")?);
        }

        candidates
    };

    for (id, path) in candidates {
        let Some(created_at) = local_file_timestamp(Path::new(&path)) else {
            continue;
        };

        connection
            .execute(
                "UPDATE upload_history SET created_at = ?1 WHERE id = ?2 AND created_at = 0",
                params![created_at, id],
            )
            .context("failed to backfill a local capture date")?;
    }

    Ok(())
}

fn local_file_timestamp(path: &Path) -> Option<i64> {
    let metadata = path_metadata(path).ok()?;

    if !metadata.is_file() {
        return None;
    }

    let timestamp = metadata.created().or_else(|_| metadata.modified()).ok()?;
    let seconds = timestamp
        .into_std()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs();

    i64::try_from(seconds).ok()
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

/// Return the default user-visible directory for retained captures.
///
/// # Errors
///
/// Returns an error when no suitable user or application directory is available.
pub fn default_capture_directory() -> Result<PathBuf> {
    if let Some(pictures) = UserDirs::new()
        .and_then(|directories| directories.picture_dir().map(std::path::Path::to_path_buf))
    {
        return Ok(pictures.join("ShareR"));
    }

    ProjectDirs::from("re", "", "sharer")
        .map(|directories| directories.data_local_dir().join("captures"))
        .context("capture directory unavailable")
}

/// Return whether a configured capture root must be replaced with the platform default.
#[must_use]
pub fn capture_directory_needs_default(configured: &str) -> bool {
    let configured = configured.trim();

    configured.is_empty() || !Path::new(configured).is_absolute()
}

/// Resolve an empty or relative capture-directory setting to the platform default.
///
/// # Errors
///
/// Returns an error when a default directory is required but unavailable.
pub fn capture_directory_or_default(configured: &str) -> Result<PathBuf> {
    if capture_directory_needs_default(configured) {
        default_capture_directory()
    } else {
        Ok(PathBuf::from(configured.trim()))
    }
}

/// Return the year-month folder used for a newly generated capture.
#[must_use]
pub fn monthly_capture_directory(root: &Path) -> PathBuf {
    root.join(chrono::Local::now().format("%Y-%m").to_string())
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

#[cfg(test)]
mod tests {
    use std::io::{Error, ErrorKind};

    use super::{
        LocalPathState, PlatformStorage, capture_directory_needs_default,
        capture_directory_or_default, classify_local_path_metadata, default_capture_directory,
    };

    #[test]
    fn relative_capture_directories_fall_back_to_the_user_pictures_folder() {
        assert!(capture_directory_needs_default(""));
        assert!(capture_directory_needs_default("."));
        assert!(capture_directory_needs_default("captures"));
        assert!(!capture_directory_needs_default(
            std::env::temp_dir().to_string_lossy().as_ref()
        ));
    }

    #[test]
    fn an_empty_capture_directory_resolves_to_the_platform_default() {
        assert_eq!(
            capture_directory_or_default("  ").unwrap(),
            default_capture_directory().unwrap()
        );

        let explicit = std::env::temp_dir().join("sharer-explicit-captures");

        assert_eq!(
            capture_directory_or_default(explicit.to_string_lossy().as_ref()).unwrap(),
            explicit
        );
    }

    #[test]
    fn only_not_found_metadata_is_classified_as_missing() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("capture.png");

        std::fs::write(&file, b"capture").unwrap();

        assert_eq!(
            classify_local_path_metadata(
                std::fs::metadata(&file).map(|metadata| metadata.is_file())
            ),
            LocalPathState::RegularFile
        );
        assert_eq!(
            classify_local_path_metadata(
                std::fs::metadata(directory.path()).map(|metadata| metadata.is_file())
            ),
            LocalPathState::NotRegular
        );
        assert_eq!(
            classify_local_path_metadata(Err(Error::new(ErrorKind::NotFound, "injected"))),
            LocalPathState::Missing
        );
        assert_eq!(
            classify_local_path_metadata(Err(Error::new(ErrorKind::PermissionDenied, "injected"))),
            LocalPathState::Unavailable
        );
    }

    #[test]
    fn cancelling_reconciliation_at_each_boundary_never_persists_a_partial_batch() {
        use crate::history::HistoryEntry;

        for cancel_at in 0..=6 {
            let directory = tempfile::tempdir().unwrap();
            let storage = PlatformStorage::open_in(directory.path()).unwrap();
            let mut entries = Vec::new();

            for name in ["first.png", "second.png"] {
                let mut entry =
                    HistoryEntry::local(name.to_owned(), 1, &directory.path().join(name));

                entry.id = storage.insert_history(&entry).unwrap();
                entries.push(entry);
            }

            let original = entries.clone();
            let mut boundary = 0;
            let report = storage
                .reconcile_missing_history_local_paths_cancellable(&mut entries, || {
                    let cancelled = boundary == cancel_at;

                    boundary += 1;

                    cancelled
                })
                .unwrap();

            assert!(report.cleared == 0 || report.cleared == 2);

            for (entry, original) in entries.iter().zip(&original) {
                assert_eq!(storage.history_entry(entry.id).unwrap().unwrap(), *entry);

                if report.cleared == 0 {
                    assert_eq!(entry.local_path, original.local_path);
                } else {
                    assert!(entry.local_path.is_empty());
                }
            }
        }
    }
}
