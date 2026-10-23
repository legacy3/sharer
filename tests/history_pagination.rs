use sharer::{
    MAX_HISTORY_PAGE_SIZE,
    config::{AppConfig, CaptureNameMode},
    history::{HistoryEntry, HistoryFilter, HistorySort, HistorySortColumn, SortDirection},
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
        upload_header_endpoint: Some("https://uploads.example/files".to_owned()),
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
fn legacy_custom_header_is_bound_to_its_existing_endpoint_on_load() {
    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();

    storage
        .write(
            "application",
            br#"{"uploader_url":"https://uploads.example:443/upload","upload_header_name":"X-Api-Key","upload_header_value":"fake-secret"}"#,
        )
        .unwrap();

    let mut config = AppConfig::load(&storage).unwrap();

    assert_eq!(
        config.upload_header_endpoint.as_deref(),
        Some("https://uploads.example/upload")
    );
    assert!(config.upload_target().validate().is_ok());

    config.uploader_url = "https://other.example/upload".to_owned();
    assert!(config.upload_target().validate().is_err());
    config.save(&storage).unwrap();
    assert!(
        AppConfig::load(&storage)
            .unwrap()
            .upload_target()
            .validate()
            .is_err()
    );
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

#[test]
fn history_search_combines_visible_fields_with_upload_filters() {
    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();
    let entries = [
        HistoryEntry {
            filename: "Review notes.png".to_owned(),
            link: String::new(),
            local_path: "/captures/notes.png".to_owned(),
            ..entry(1)
        },
        HistoryEntry {
            filename: "remote.png".to_owned(),
            link: "https://files.example/REVIEW/remote".to_owned(),
            ..entry(2)
        },
        HistoryEntry {
            filename: "path.png".to_owned(),
            local_path: "/captures/reView/path.png".to_owned(),
            ..entry(3)
        },
        HistoryEntry {
            filename: "Review missing local file.png".to_owned(),
            link: String::new(),
            ..entry(4)
        },
        entry(5),
    ];

    for item in &entries {
        storage.insert_history(item).unwrap();
    }

    for (filter, indices) in [
        (HistoryFilter::All, vec![3, 2, 1, 0]),
        (HistoryFilter::LocalOnly, vec![3, 0]),
        (HistoryFilter::Uploaded, vec![2, 1]),
    ] {
        let matches = storage
            .history_page_filtered(0, 25, HistorySort::default(), " \t rEvIeW \n", filter)
            .unwrap();
        let expected: Vec<_> = indices
            .into_iter()
            .map(|index| entries[index].filename.as_str())
            .collect();

        assert_eq!(
            matches
                .iter()
                .map(|item| item.filename.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            storage
                .history_count_filtered(" \t rEvIeW \n", filter)
                .unwrap(),
            u64::try_from(expected.len()).unwrap()
        );
    }

    assert_eq!(storage.history_count().unwrap(), 5);
    assert_eq!(
        storage
            .history_count_filtered("", HistoryFilter::LocalOnly)
            .unwrap(),
        2
    );
    assert_eq!(
        storage
            .history_count_filtered("", HistoryFilter::Uploaded)
            .unwrap(),
        3
    );
}

#[test]
fn history_search_treats_sql_wildcards_and_quotes_as_literal_text() {
    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();
    let filename = "100%_D'Angelo\\capture.png";

    for name in [filename, "100percentXDangelo.png"] {
        storage
            .insert_history(&HistoryEntry {
                filename: name.to_owned(),
                ..entry(1)
            })
            .unwrap();
    }

    for query in ["%", "_", "'", "\\", "100%_d'angelo"] {
        let matches = storage
            .history_page_filtered(0, 25, HistorySort::default(), query, HistoryFilter::All)
            .unwrap();

        assert_eq!(matches.len(), 1, "query: {query}");
        assert_eq!(matches[0].filename, filename);
        assert_eq!(
            storage
                .history_count_filtered(query, HistoryFilter::All)
                .unwrap(),
            1
        );
    }

    let injection = "x' OR 1=1 --";

    assert!(
        storage
            .history_page_filtered(0, 25, HistorySort::default(), injection, HistoryFilter::All)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        storage
            .history_count_filtered(injection, HistoryFilter::All)
            .unwrap(),
        0
    );
    assert_eq!(storage.history_count().unwrap(), 2);
}

#[test]
fn history_search_does_not_match_delete_tokens_or_expiration_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();

    storage
        .insert_history(&HistoryEntry {
            delete_url: "https://files.example/delete/private-delete-token".to_owned(),
            expires_at: "provider-expiration-metadata".to_owned(),
            ..entry(1)
        })
        .unwrap();

    for query in ["private-delete-token", "provider-expiration-metadata"] {
        assert!(
            storage
                .history_page_filtered(0, 25, HistorySort::default(), query, HistoryFilter::All)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            storage
                .history_count_filtered(query, HistoryFilter::All)
                .unwrap(),
            0
        );
    }
}

#[test]
fn blank_history_search_preserves_the_existing_page_and_count_results() {
    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();

    for index in 0..60 {
        storage.insert_history(&entry(index)).unwrap();
    }

    for query in ["", " \t\r\n "] {
        assert_eq!(
            storage
                .history_count_filtered(query, HistoryFilter::default())
                .unwrap(),
            storage.history_count().unwrap()
        );

        for offset in [0, 25, 50, 75] {
            assert_eq!(
                storage
                    .history_page_filtered(
                        offset,
                        25,
                        HistorySort::default(),
                        query,
                        HistoryFilter::default()
                    )
                    .unwrap(),
                storage.history_page(offset, 25).unwrap()
            );
        }
    }
}

#[test]
fn filtered_history_pages_keep_ties_stable_for_every_sort_order() {
    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();
    let mut all_ids = Vec::new();
    let mut local_ids = Vec::new();
    let mut uploaded_ids = Vec::new();

    for index in 0..61 {
        let local_only = index % 2 == 0;
        let id = storage
            .insert_history(&HistoryEntry {
                filename: "same-name.png".to_owned(),
                local_path: "/captures/matching-query/same-name.png".to_owned(),
                link: if local_only {
                    String::new()
                } else {
                    "https://files.example/same".to_owned()
                },
                ..entry(1)
            })
            .unwrap();

        all_ids.push(id);

        if local_only {
            local_ids.push(id);
        } else {
            uploaded_ids.push(id);
        }
    }

    storage.insert_history(&entry(2)).unwrap();

    for column in [
        HistorySortColumn::Filename,
        HistorySortColumn::CreatedAt,
        HistorySortColumn::Size,
        HistorySortColumn::ExpiresAt,
    ] {
        for direction in [SortDirection::Ascending, SortDirection::Descending] {
            for (filter, ids) in [
                (HistoryFilter::All, &all_ids),
                (HistoryFilter::LocalOnly, &local_ids),
                (HistoryFilter::Uploaded, &uploaded_ids),
            ] {
                let sort = HistorySort { column, direction };
                let mut expected = ids.clone();
                let mut actual = Vec::new();

                if direction == SortDirection::Descending {
                    expected.reverse();
                }

                assert_eq!(
                    storage
                        .history_count_filtered("matching-query", filter)
                        .unwrap(),
                    u64::try_from(expected.len()).unwrap()
                );

                for offset in (0..expected.len()).step_by(25) {
                    let page = storage
                        .history_page_filtered(offset, 25, sort, "matching-query", filter)
                        .unwrap();

                    assert_eq!(page.len(), 25.min(expected.len() - offset));
                    actual.extend(page.into_iter().map(|item| item.id));
                }

                assert_eq!(actual, expected, "{sort:?}, {filter:?}");
                assert!(
                    storage
                        .history_page_filtered(expected.len(), 25, sort, "matching-query", filter)
                        .unwrap()
                        .is_empty()
                );
            }
        }
    }
}

#[test]
fn filtered_history_rejects_invalid_page_bounds() {
    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();

    for limit in [0, MAX_HISTORY_PAGE_SIZE + 1, usize::MAX] {
        assert!(
            storage
                .history_page_filtered(0, limit, HistorySort::default(), "", HistoryFilter::All)
                .is_err()
        );
    }

    if let Some(offset) = usize::try_from(i64::MAX)
        .ok()
        .and_then(|value| value.checked_add(1))
    {
        assert!(
            storage
                .history_page_filtered(offset, 25, HistorySort::default(), "", HistoryFilter::All)
                .is_err()
        );
    }
}

fn insert_cursor_fixture(storage: &PlatformStorage) {
    for index in 0..85 {
        let names = [
            "Alpha.png",
            "alpha.PNG",
            "bravo.png",
            "CHARLIE.png",
            "charlie.PNG",
        ];
        let expiration = match index % 4 {
            0 => "",
            1 | 2 => "2026-10-22T12:00:00Z",
            _ => "2026-10-23T12:00:00Z",
        };
        let item = HistoryEntry {
            filename: names[index % names.len()].to_owned(),
            created_at: i64::try_from(index % 3).unwrap(),
            size_bytes: u64::try_from(index % 3).unwrap(),
            expires_at: expiration.to_owned(),
            local_path: if index % 7 == 0 {
                "/captures/unrelated/file.png".to_owned()
            } else {
                "/captures/needle/file.png".to_owned()
            },
            link: if index % 2 == 0 {
                String::new()
            } else {
                "https://files.example/capture".to_owned()
            },
            ..entry(u64::try_from(index).unwrap())
        };

        storage.insert_history(&item).unwrap();
    }
}

#[test]
fn history_cursor_matches_every_sorted_archive_with_case_ties_and_missing_expiry() {
    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();

    insert_cursor_fixture(&storage);

    for column in [
        HistorySortColumn::Filename,
        HistorySortColumn::CreatedAt,
        HistorySortColumn::Size,
        HistorySortColumn::ExpiresAt,
    ] {
        for direction in [SortDirection::Ascending, SortDirection::Descending] {
            let sort = HistorySort { column, direction };

            for filter in [
                HistoryFilter::All,
                HistoryFilter::LocalOnly,
                HistoryFilter::Uploaded,
            ] {
                let expected = storage
                    .history_page_filtered(0, MAX_HISTORY_PAGE_SIZE, sort, "needle", filter)
                    .unwrap();
                let mut actual = storage
                    .history_page_filtered(0, 7, sort, "needle", filter)
                    .unwrap();

                while let Some(cursor) = actual.last() {
                    let next = storage
                        .history_page_filtered_after(7, sort, "needle", filter, cursor)
                        .unwrap();

                    assert!(next.len() <= 7);

                    if next.is_empty() {
                        break;
                    }

                    actual.extend(next);
                    assert!(
                        actual.len() <= expected.len(),
                        "cursor repeated a page: {sort:?}"
                    );
                }

                assert_eq!(actual, expected, "sort: {sort:?}, filter: {filter:?}");
            }
        }
    }
}

#[test]
fn history_cursor_keeps_remaining_matches_when_prior_paths_or_records_are_removed() {
    for column in [
        HistorySortColumn::Filename,
        HistorySortColumn::CreatedAt,
        HistorySortColumn::Size,
        HistorySortColumn::ExpiresAt,
    ] {
        for direction in [SortDirection::Ascending, SortDirection::Descending] {
            let directory = tempfile::tempdir().unwrap();
            let storage = PlatformStorage::open_in(directory.path()).unwrap();
            let sort = HistorySort { column, direction };

            insert_cursor_fixture(&storage);

            let expected = storage
                .history_page_filtered(
                    0,
                    MAX_HISTORY_PAGE_SIZE,
                    sort,
                    "needle",
                    HistoryFilter::LocalOnly,
                )
                .unwrap();
            let first = storage
                .history_page_filtered(0, 7, sort, "needle", HistoryFilter::LocalOnly)
                .unwrap();
            let cursor = first.last().unwrap().clone();

            for item in &first {
                storage.clear_history_local_path(item.id).unwrap();
            }

            storage.remove_history(cursor.id).unwrap();

            let mut remaining = storage
                .history_page_filtered_after(7, sort, "needle", HistoryFilter::LocalOnly, &cursor)
                .unwrap();

            while let Some(cursor) = remaining.last() {
                let next = storage
                    .history_page_filtered_after(
                        7,
                        sort,
                        "needle",
                        HistoryFilter::LocalOnly,
                        cursor,
                    )
                    .unwrap();

                if next.is_empty() {
                    break;
                }

                remaining.extend(next);
                assert!(remaining.len() <= expected.len());
            }

            assert_eq!(remaining, expected[7..], "sort: {sort:?}");
            assert_eq!(
                storage
                    .history_count_filtered("needle", HistoryFilter::LocalOnly)
                    .unwrap(),
                u64::try_from(expected.len() - 7).unwrap()
            );
        }
    }
}

#[test]
fn history_cursor_rejects_invalid_limits_and_unrepresentable_sizes() {
    let directory = tempfile::tempdir().unwrap();
    let storage = PlatformStorage::open_in(directory.path()).unwrap();
    let cursor = entry(1);

    for limit in [0, MAX_HISTORY_PAGE_SIZE + 1, usize::MAX] {
        assert!(
            storage
                .history_page_filtered_after(
                    limit,
                    HistorySort::default(),
                    "",
                    HistoryFilter::All,
                    &cursor
                )
                .is_err()
        );
    }

    let invalid_size = HistoryEntry {
        size_bytes: u64::MAX,
        ..cursor
    };

    assert!(
        storage
            .history_page_filtered_after(
                25,
                HistorySort {
                    column: HistorySortColumn::Size,
                    direction: SortDirection::Ascending
                },
                "",
                HistoryFilter::All,
                &invalid_size
            )
            .is_err()
    );
}
