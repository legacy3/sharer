use sharer::{
    config::AppConfig,
    history::HistoryEntry,
    storage::{PlatformStorage, Storage as _},
};

fn entry(index: u64) -> HistoryEntry {
    HistoryEntry {
        link: format!("https://files.example/{index}"),
        delete_url: format!("https://files.example/delete/{index}"),
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
    expected.privacy.private_capture_names = true;
    expected.privacy.remove_exif = true;

    expected.save(&storage).unwrap();

    assert_eq!(AppConfig::load(&storage).unwrap(), expected);
}
