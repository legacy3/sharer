//! Regression tests for history presentation and stable action identities.

use std::{cell::RefCell, rc::Rc};

use sharer::history::HistorySort;
use slint::{Model as _, VecModel};

use super::{
    HistoryController, HistoryRow, clear_local_row, row_by_token, row_from_entry,
    wait_until_idle_for_test,
};

struct HistoryTestPlatform;

impl slint::platform::Platform for HistoryTestPlatform {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::platform::PlatformError> {
        use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};

        Ok(MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer))
    }
}

#[test]
fn search_and_filter_callbacks_reset_pages_and_expire_old_action_tokens() {
    use sharer::{history::HistoryEntry, storage::PlatformStorage};

    slint::platform::set_platform(Box::new(HistoryTestPlatform)).ok();
    let window = crate::AppWindow::new().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let storage = Rc::new(PlatformStorage::open_in(directory.path()).unwrap());

    for index in 0..60 {
        storage
            .insert_history(&HistoryEntry {
                id: 0,
                created_at: index + 1,
                filename: format!("capture-{index}.png"),
                size_bytes: 42,
                link: if index % 2 == 0 {
                    format!("https://public.example/{index}")
                } else {
                    String::new()
                },
                delete_url: "https://delete.example/private-token".to_owned(),
                local_path: String::new(),
                expires_at: String::new(),
            })
            .unwrap();
    }

    let deleted: super::LocalDeletionObserver = Rc::new(|_, _, _| {});
    let removed: super::HistoryRemovalObserver = Rc::new(|_, _| {});
    let controller = super::initialize(&window, &storage, false, &deleted, &removed).unwrap();

    window.invoke_history_next();
    wait_until_idle_for_test(&window, &controller);
    assert_eq!(controller.borrow().page, 1);
    window.invoke_history_query_changed("".into(), 0);
    wait_until_idle_for_test(&window, &controller);
    assert_eq!(
        controller.borrow().page,
        1,
        "reopening an unchanged search retains its page"
    );
    let previous_token = window.get_history_rows().row_data(0).unwrap().action_token;

    window.invoke_history_query_changed("capture".into(), 2);
    wait_until_idle_for_test(&window, &controller);
    assert_eq!(controller.borrow().page, 0);
    assert_eq!(controller.borrow().total, 30);
    assert_eq!(window.get_history_rows().row_count(), 25);
    assert_eq!(window.get_history_page_label(), "30 matches · Page 1 of 2");
    assert!(!window.get_history_has_previous());
    assert!(window.get_history_has_next());
    assert!(row_by_token(&controller, previous_token.as_str()).is_none());
    assert!(
        window
            .get_history_rows()
            .iter()
            .all(|row| !row.link.is_empty())
    );

    window.invoke_history_next();
    wait_until_idle_for_test(&window, &controller);
    assert_eq!(window.get_history_rows().row_count(), 5);
    window.invoke_history_sort_column_clicked(0);
    wait_until_idle_for_test(&window, &controller);
    assert_eq!(controller.borrow().page, 0);
    assert_eq!(window.get_history_filter(), 2);
    assert_eq!(window.get_history_search_query(), "capture");

    window.invoke_history_next();
    wait_until_idle_for_test(&window, &controller);
    window.invoke_history_query_changed("private-token".into(), 0);
    wait_until_idle_for_test(&window, &controller);
    assert_eq!(controller.borrow().page, 0);
    assert_eq!(controller.borrow().total, 0);
    assert_eq!(window.get_history_rows().row_count(), 0);
    assert_eq!(window.get_history_page_label(), "0 matches · Page 1 of 1");
    assert!(!window.get_history_has_next());

    window.invoke_history_query_changed("".into(), 1);
    wait_until_idle_for_test(&window, &controller);
    assert_eq!(controller.borrow().total, 30);
    assert!(
        window
            .get_history_rows()
            .iter()
            .all(|row| row.link.is_empty())
    );
    window.invoke_history_query_changed("".into(), 0);
    wait_until_idle_for_test(&window, &controller);
    assert_eq!(controller.borrow().total, 60);
    assert_eq!(window.get_history_page_label(), "60 items · Page 1 of 3");

    // A receipt or page refresh must not overwrite text waiting for its debounce.
    window.set_history_search_query("capture-5".into());
    super::observe_inserted(&window, &storage, &controller).unwrap();
    wait_until_idle_for_test(&window, &controller);
    assert_eq!(window.get_history_search_query(), "capture-5");
    window.invoke_history_query_changed(window.get_history_search_query(), 0);
    wait_until_idle_for_test(&window, &controller);
    assert_eq!(controller.borrow().page, 0);
    assert_eq!(controller.borrow().total, 11);
}

#[test]
fn searched_missing_paths_are_reconciled_across_replacement_pages() {
    use std::cell::Cell;

    use sharer::{history::HistoryEntry, storage::PlatformStorage};

    slint::platform::set_platform(Box::new(HistoryTestPlatform)).ok();
    let window = crate::AppWindow::new().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let storage = Rc::new(PlatformStorage::open_in(directory.path()).unwrap());
    let deleted_count = Rc::new(Cell::new(0));
    let observed_count = Rc::clone(&deleted_count);
    let deleted: super::LocalDeletionObserver = Rc::new(move |_, _, _| {
        observed_count.set(observed_count.get() + 1);
    });
    let removed: super::HistoryRemovalObserver = Rc::new(|_, _| {});

    for index in 0..61 {
        let path = directory
            .path()
            .join("missing-batch")
            .join(format!("{index}.png"));

        storage
            .insert_history(&HistoryEntry::local(
                format!("capture-{index}.png"),
                1,
                &path,
            ))
            .unwrap();
    }

    let controller = super::initialize(&window, &storage, true, &deleted, &removed).unwrap();

    window.invoke_history_query_changed("missing-batch".into(), 1);
    wait_until_idle_for_test(&window, &controller);
    assert_eq!(deleted_count.get(), 61);
    assert_eq!(controller.borrow().total, 0);
    assert_eq!(window.get_history_rows().row_count(), 0);
    assert!(!window.get_history_has_next());
    assert_eq!(storage.history_count().unwrap(), 61);

    for offset in [0, 25, 50] {
        assert!(
            storage
                .history_page(offset, 25)
                .unwrap()
                .iter()
                .all(|entry| entry.local_path.is_empty())
        );
    }
}

#[test]
fn refreshing_after_local_deletion_recounts_search_and_clamps_page() {
    use sharer::{history::HistoryEntry, storage::PlatformStorage};

    slint::platform::set_platform(Box::new(HistoryTestPlatform)).ok();
    let window = crate::AppWindow::new().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let storage = Rc::new(PlatformStorage::open_in(directory.path()).unwrap());
    let captures = directory.path().join("searchable-parent");

    std::fs::create_dir(&captures).unwrap();

    for index in 0..26 {
        let path = captures.join(format!("{index}.png"));

        std::fs::write(&path, b"synthetic capture").unwrap();
        storage
            .insert_history(&HistoryEntry::local(
                format!("capture-{index}.png"),
                17,
                &path,
            ))
            .unwrap();
    }

    let deleted: super::LocalDeletionObserver = Rc::new(|_, _, _| {});
    let removed: super::HistoryRemovalObserver = Rc::new(|_, _| {});
    let controller = super::initialize(&window, &storage, false, &deleted, &removed).unwrap();

    window.invoke_history_query_changed("searchable-parent".into(), 1);
    wait_until_idle_for_test(&window, &controller);
    window.invoke_history_next();
    wait_until_idle_for_test(&window, &controller);
    assert_eq!(controller.borrow().page, 1);
    let row = window.get_history_rows().row_data(0).unwrap();
    let result = super::delete_local_capture(
        &row,
        super::DeleteLocalCallbacks {
            mark_deleted: |id: i64, _: &std::path::Path| {
                clear_local_row(&controller, &id.to_string());
            },
            clear_persisted: |id| storage.clear_history_local_path(id),
        },
    )
    .unwrap();

    assert!(result.is_none());
    super::refresh_current(&window, &storage, &controller).unwrap();
    wait_until_idle_for_test(&window, &controller);
    assert_eq!(controller.borrow().page, 0);
    assert_eq!(controller.borrow().total, 25);
    assert_eq!(window.get_history_rows().row_count(), 25);
    assert!(!window.get_history_has_next());
    assert!(row_by_token(&controller, row.action_token.as_str()).is_none());
    assert_eq!(storage.history_count().unwrap(), 26);
}

#[test]
fn history_presentation_retains_raw_action_values() {
    let entry = sharer::history::HistoryEntry {
        id: i64::MAX,
        created_at: 0,
        filename: "image\n\u{202e}gnp.exe".to_owned(),
        expires_at: "soon\u{1b}[2J".to_owned(),
        link: "https://public.example/\u{202e}link".to_owned(),
        delete_url: "https://files.example/delete/fake".to_owned(),
        local_path: "folder/image\n.png".to_owned(),
        size_bytes: 42,
    };
    let row = row_from_entry(&entry, 1);

    assert_eq!(row.entry_id, i64::MAX.to_string());
    assert_eq!(row.action_token, format!("1:{}", i64::MAX));
    assert_eq!(row.filename, "image\\n\\u{202e}gnp.exe");
    assert_eq!(row.expires, "soon\\u{1b}[2J");
    assert_eq!(row.display_link, "https://public.example/\\u{202e}link");
    assert_eq!(row.display_local_path, "folder/image\\n.png");
    assert_eq!(row.link, entry.link);
    assert_eq!(row.delete_link, entry.delete_url);
    assert_eq!(row.local_path, entry.local_path);
}

#[test]
fn action_tokens_survive_reorder_and_reject_missing_or_reused_ids() {
    let original = HistoryRow {
        entry_id: "2".into(),
        action_token: "1:2".into(),
        local_path: "original.png".into(),
        display_local_path: "original.png".into(),
        ..Default::default()
    };
    let replacement = HistoryRow {
        entry_id: "42".into(),
        action_token: "1:42".into(),
        local_path: "replacement.png".into(),
        display_local_path: "replacement.png".into(),
        ..Default::default()
    };
    let rows = Rc::new(VecModel::from(vec![original.clone(), replacement.clone()]));
    let directory = tempfile::tempdir().unwrap();
    let storage = sharer::storage::PlatformStorage::open_in(directory.path()).unwrap();
    let controller = RefCell::new(HistoryController {
        loader: Rc::new(RefCell::new(super::HistoryLoader::new(&storage).unwrap())),
        poll_timer: slint::Timer::default(),
        checking: false,
        loaded: true,
        local_deleted: Rc::new(|_, _, _| {}),
        generation: 1,
        page: 0,
        total: 2,
        rows: Rc::clone(&rows),
        sort: HistorySort::default(),
        search: super::HistorySearch::default(),
        suspended: false,
    });
    // Menus capture this token when they open, before a possible model change.
    let action_token = original.action_token.clone();
    let entry_id = original.entry_id.clone();

    rows.set_vec(vec![replacement.clone(), original]);
    assert_eq!(
        row_by_token(&controller, action_token.as_str())
            .unwrap()
            .local_path,
        "original.png"
    );
    clear_local_row(&controller, entry_id.as_str());
    assert_eq!(rows.row_data(0).unwrap().local_path, "replacement.png");
    assert!(rows.row_data(1).unwrap().local_path.is_empty());
    assert!(rows.row_data(1).unwrap().display_local_path.is_empty());

    rows.set_vec(vec![replacement]);
    assert!(row_by_token(&controller, action_token.as_str()).is_none());
    clear_local_row(&controller, entry_id.as_str());
    assert_eq!(rows.row_data(0).unwrap().local_path, "replacement.png");

    // SQLite can reuse an ID after deletion; a new model generation must reject old menus.
    rows.set_vec(vec![HistoryRow {
        entry_id: entry_id.clone(),
        action_token: format!("2:{entry_id}").into(),
        local_path: "reused-id.png".into(),
        ..Default::default()
    }]);
    assert!(row_by_token(&controller, action_token.as_str()).is_none());
    assert_eq!(
        row_by_token(&controller, &format!("2:{entry_id}"))
            .unwrap()
            .local_path,
        "reused-id.png"
    );
}

#[test]
fn cancelled_search_rejects_stale_rows_but_not_committed_deletion_notifications() {
    use super::worker::{HistoryEvent, HistoryPage, Reply};
    use sharer::{history::HistoryEntry, storage::PlatformStorage};
    use std::cell::Cell;

    slint::platform::set_platform(Box::new(HistoryTestPlatform)).ok();
    let window = crate::AppWindow::new().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let storage = Rc::new(PlatformStorage::open_in(directory.path()).unwrap());
    let entry = HistoryEntry::local("retained.png".to_owned(), 1, std::path::Path::new(""));

    storage.insert_history(&entry).unwrap();
    let deleted_count = Rc::new(Cell::new(0));
    let observed_count = Rc::clone(&deleted_count);
    let deleted: super::LocalDeletionObserver =
        Rc::new(move |_, _, _| observed_count.set(observed_count.get() + 1));
    let removed: super::HistoryRemovalObserver = Rc::new(|_, _| {});
    let controller = super::initialize(&window, &storage, true, &deleted, &removed).unwrap();

    window.invoke_history_query_changed("retained".into(), 0);
    let stale_generation = controller.borrow().generation;

    assert!(window.get_history_checking());
    window.invoke_history_cancel_check();
    assert!(!window.get_history_checking());
    assert_eq!(window.get_history_load_message(), "Search cancelled");
    super::apply_reply(
        &window,
        &controller,
        Reply {
            generation: stale_generation,
            event: HistoryEvent::Complete(Ok(HistoryPage {
                page: 0,
                total: 1,
                entries: vec![entry],
                reconciliation: sharer::storage::HistoryLocalPathReconciliation::default(),
            })),
        },
    );
    assert_eq!(window.get_history_rows().row_count(), 0);
    super::apply_reply(
        &window,
        &controller,
        Reply {
            generation: stale_generation,
            event: HistoryEvent::Progress {
                cleared: vec![(1, "gone.png".into())],
                checked: 1,
            },
        },
    );
    assert_eq!(deleted_count.get(), 1);
    assert_eq!(window.get_history_load_message(), "Search cancelled");

    // Identical text is retryable after cancellation, and only the newest query can finish.
    window.invoke_history_query_changed("retained".into(), 0);
    assert!(window.get_history_checking());
    window.invoke_history_query_changed("no-match".into(), 0);
    wait_until_idle_for_test(&window, &controller);
    assert_eq!(window.get_history_rows().row_count(), 0);
    assert_eq!(window.get_history_page_label(), "0 matches · Page 1 of 1");
    assert!(window.get_history_load_message().is_empty());
    window.invoke_history_query_changed("retained".into(), 0);
    wait_until_idle_for_test(&window, &controller);
    assert_eq!(window.get_history_rows().row_count(), 1);
    assert_eq!(storage.history_count().unwrap(), 1);
}
