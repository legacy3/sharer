use sharer::{
    config::{AppConfig, CaptureNameMode},
    history::{HistoryEntry, HistorySort, HistorySortColumn, SortDirection},
    storage::{PlatformStorage, Storage as _},
};

fn entry(index: u64) -> HistoryEntry {
    HistoryEntry {
        id: 0,
        created_at: i64::try_from(index).unwrap() + 1,
        link: format!("https://files.example/{index}"),
        delete_url: format!("https://files.example/delete/{index}"),
        local_path: String::new(),
        filename: format!("capture-{index}.png"),
        size_bytes: index,
        expires_at: "2026-09-09T00:00:00Z".to_owned(),
    }
}

#[test]
fn settings_and_all_history_share_one_database() {
    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();

    storage
        .write("application", br#"{"configured":true}"#)
        .unwrap();

    for index in 0..60 {
        storage.insert_history(&entry(index)).unwrap();
    }

    let first_page = storage.history_page(0, 25).unwrap();
    let last_page = storage.history_page(50, 25).unwrap();

    assert_eq!(storage.history_count().unwrap(), 60);
    assert_eq!(first_page.len(), 25);
    assert_eq!(first_page[0].filename, "capture-59.png");
    assert_eq!(last_page.len(), 10);
    assert_eq!(last_page[9].filename, "capture-0.png");

    let oldest_page = storage
        .history_page_sorted(
            0,
            25,
            HistorySort {
                column: HistorySortColumn::CreatedAt,
                direction: SortDirection::Ascending,
            },
        )
        .unwrap();

    assert_eq!(oldest_page[0].filename, "capture-0.png");
    assert_eq!(oldest_page[24].filename, "capture-24.png");
    assert_eq!(
        storage.read("application").unwrap().unwrap(),
        br#"{"configured":true}"#
    );
    assert!(directory.path().join("sharer.sqlite3").is_file());
}

#[test]
fn configuration_round_trips_through_sqlite() {
    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();
    let mut expected = AppConfig {
        uploader_url: "https://uploads.example/files".to_owned(),
        upload_header_name: "Authorization".to_owned(),
        upload_header_value: "Bearer private".to_owned(),
        ..AppConfig::default()
    };

    expected.behavior.force_sdr_captures = true;
    expected.privacy.remove_exif = true;

    expected.save(&storage).unwrap();

    assert_eq!(AppConfig::load(&storage).unwrap(), expected);
}

#[test]
fn legacy_private_name_setting_migrates_to_the_naming_selector() {
    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();

    storage
        .write(
            "application",
            br#"{"private_capture_names":true,"capture_naming":{"mode":"friendly","custom_name":"screenshot"}}"#,
        )
        .unwrap();

    let config = AppConfig::load(&storage).unwrap();

    assert_eq!(config.capture_naming.mode, CaptureNameMode::Random);
    assert!(config.capture_naming.custom_name.is_empty());
    assert!(!config.privacy.private_capture_names);
}

#[test]
fn history_can_sort_by_every_visible_data_column() {
    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();
    let entries = [
        HistoryEntry {
            filename: "bravo.png".to_owned(),
            created_at: 20,
            size_bytes: 300,
            expires_at: "2026-09-20T00:00:00Z".to_owned(),
            ..entry(1)
        },
        HistoryEntry {
            filename: "Alpha.png".to_owned(),
            created_at: 30,
            size_bytes: 100,
            expires_at: String::new(),
            ..entry(2)
        },
        HistoryEntry {
            filename: "charlie.png".to_owned(),
            created_at: 10,
            size_bytes: 200,
            expires_at: "2026-09-18T00:00:00Z".to_owned(),
            ..entry(3)
        },
    ];

    for entry in &entries {
        storage.insert_history(entry).unwrap();
    }

    let filenames = storage
        .history_page_sorted(
            0,
            25,
            HistorySort {
                column: HistorySortColumn::Filename,
                direction: SortDirection::Ascending,
            },
        )
        .unwrap();

    assert_eq!(filenames[0].filename, "Alpha.png");
    assert_eq!(filenames[2].filename, "charlie.png");

    let sizes = storage
        .history_page_sorted(
            0,
            25,
            HistorySort {
                column: HistorySortColumn::Size,
                direction: SortDirection::Descending,
            },
        )
        .unwrap();

    assert_eq!(sizes[0].size_bytes, 300);
    assert_eq!(sizes[2].size_bytes, 100);

    let expirations = storage
        .history_page_sorted(
            0,
            25,
            HistorySort {
                column: HistorySortColumn::ExpiresAt,
                direction: SortDirection::Ascending,
            },
        )
        .unwrap();

    assert_eq!(expirations[0].expires_at, "2026-09-18T00:00:00Z");
    assert!(expirations[2].expires_at.is_empty());
}

#[test]
fn local_paths_and_history_mutations_persist() {
    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();
    let local_path = directory.path().join("2026-09").join("capture.png");
    let mut local = HistoryEntry::local("capture.png".to_owned(), 42, &local_path);

    local.id = storage.insert_history(&local).unwrap();

    let loaded = storage.history_page(0, 25).unwrap();

    assert_eq!(loaded[0], local);

    storage.clear_history_local_path(local.id).unwrap();
    assert!(
        storage.history_page(0, 25).unwrap()[0]
            .local_path
            .is_empty()
    );

    storage.remove_history(local.id).unwrap();
    assert_eq!(storage.history_count().unwrap(), 0);
}

#[test]
fn missing_local_file_is_reconciled_after_restart_without_losing_remote_data() {
    let directory = tempfile::tempdir().unwrap();
    let local_path = directory.path().join("capture.png");

    std::fs::write(&local_path, b"capture").unwrap();
    let mut saved = entry(7);

    saved.local_path = local_path.to_string_lossy().into_owned();
    let id = {
        let storage = PlatformStorage::open_in(directory.path()).unwrap();

        storage.insert_history(&saved).unwrap()
    };

    std::fs::remove_file(&local_path).unwrap();

    let storage = PlatformStorage::open_in(directory.path()).unwrap();
    let mut loaded = storage.history_page(0, 25).unwrap();

    assert_eq!(loaded[0].id, id);
    assert!(!loaded[0].local_path.is_empty());

    let reconciliation = storage
        .reconcile_missing_history_local_paths(&mut loaded)
        .unwrap();

    assert_eq!(reconciliation.cleared, 1);
    assert_eq!(reconciliation.unavailable, 0);
    assert_eq!(reconciliation.not_regular, 0);
    assert!(loaded[0].local_path.is_empty());
    assert_eq!(loaded[0].link, saved.link);
    assert_eq!(loaded[0].delete_url, saved.delete_url);

    let persisted = storage.history_page(0, 25).unwrap().remove(0);

    assert!(persisted.local_path.is_empty());
    assert_eq!(persisted.link, saved.link);
    assert_eq!(persisted.delete_url, saved.delete_url);
}

#[test]
fn existing_non_file_local_path_is_preserved_and_reported() {
    let directory = tempfile::tempdir().unwrap();
    let local_path = directory.path().join("capture-directory");

    std::fs::create_dir(&local_path).unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();
    let mut saved = entry(8);

    saved.local_path = local_path.to_string_lossy().into_owned();
    storage.insert_history(&saved).unwrap();
    let mut loaded = storage.history_page(0, 25).unwrap();

    let reconciliation = storage
        .reconcile_missing_history_local_paths(&mut loaded)
        .unwrap();

    assert_eq!(reconciliation.cleared, 0);
    assert_eq!(reconciliation.unavailable, 0);
    assert_eq!(reconciliation.not_regular, 1);
    assert_eq!(loaded[0].local_path, saved.local_path);
    assert_eq!(
        storage.history_page(0, 25).unwrap()[0].local_path,
        saved.local_path
    );
}

#[test]
fn existing_upload_history_schema_gains_capture_columns() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("sharer.sqlite3");
    let connection = rusqlite::Connection::open(database).unwrap();

    connection
        .execute_batch(
            "CREATE TABLE settings (key TEXT PRIMARY KEY NOT NULL, value BLOB NOT NULL) WITHOUT ROWID;
             CREATE TABLE upload_history (
                 id INTEGER PRIMARY KEY,
                 original_name TEXT NOT NULL,
                 size_bytes INTEGER NOT NULL,
                 link TEXT NOT NULL,
                 delete_url TEXT NOT NULL,
                 expires_at TEXT NOT NULL
             );
             INSERT INTO upload_history
                 (original_name, size_bytes, link, delete_url, expires_at)
             VALUES ('old.png', 7, 'https://example.test/old', '', 'managed');",
        )
        .unwrap();
    drop(connection);

    let storage = PlatformStorage::open_in(directory.path()).unwrap();
    let entry = storage.history_page(0, 25).unwrap().remove(0);

    assert_eq!(entry.filename, "old.png");
    assert!(entry.local_path.is_empty());
    assert_eq!(entry.created_at, 0);
}

#[test]
fn legacy_local_history_uses_the_file_timestamp() {
    let directory = tempfile::tempdir().unwrap();
    let local_file = directory.path().join("legacy.png");

    std::fs::write(&local_file, b"png").unwrap();
    let database = directory.path().join("sharer.sqlite3");
    let connection = rusqlite::Connection::open(database).unwrap();

    connection
        .execute_batch(
            "CREATE TABLE settings (key TEXT PRIMARY KEY NOT NULL, value BLOB NOT NULL) WITHOUT ROWID;
             CREATE TABLE upload_history (
                 id INTEGER PRIMARY KEY,
                 original_name TEXT NOT NULL,
                 size_bytes INTEGER NOT NULL,
                 link TEXT NOT NULL,
                 delete_url TEXT NOT NULL,
                 expires_at TEXT NOT NULL,
                 local_path TEXT NOT NULL DEFAULT ''
             );",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO upload_history
             (original_name, size_bytes, link, delete_url, expires_at, local_path)
             VALUES ('legacy.png', 3, '', '', '', ?1)",
            [local_file.to_string_lossy().as_ref()],
        )
        .unwrap();
    drop(connection);

    let storage = PlatformStorage::open_in(directory.path()).unwrap();
    let migrated = storage.history_page(0, 25).unwrap().remove(0);

    assert!(migrated.created_at > 0);
}
