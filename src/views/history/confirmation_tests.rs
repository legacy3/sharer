//! Deferred destructive actions validate the original selection and database row.

use std::{cell::RefCell, rc::Rc};

use sharer::{history::HistoryEntry, storage::PlatformStorage};
use slint::{Model as _, ModelRc};

use super::{HistoryController, HistoryRow, wait_until_idle_for_test};

struct ConfirmationTestPlatform;

impl slint::platform::Platform for ConfirmationTestPlatform {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::platform::PlatformError> {
        use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};

        Ok(MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer))
    }
}

struct Fixture {
    window: crate::AppWindow,
    storage: Rc<PlatformStorage>,
    controller: Rc<RefCell<HistoryController>>,
    entry: HistoryEntry,
    directory: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        slint::platform::set_platform(Box::new(ConfirmationTestPlatform)).ok();
        let window = crate::AppWindow::new().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let storage = Rc::new(PlatformStorage::open_in(directory.path()).unwrap());
        let path = directory.path().join("capture.png");

        std::fs::write(&path, "synthetic capture").unwrap();
        let mut entry = HistoryEntry::local("capture.png".to_owned(), 17, &path);

        entry.id = storage.insert_history(&entry).unwrap();
        let deleted: super::LocalDeletionObserver = Rc::new(|_, _, _| {});
        let removed: super::HistoryRemovalObserver = Rc::new(|_, _| {});
        let controller = super::initialize(&window, &storage, false, &deleted, &removed).unwrap();

        Self {
            window,
            storage,
            controller,
            entry,
            directory,
        }
    }

    fn row(&self) -> HistoryRow {
        let rows: ModelRc<HistoryRow> = self.window.get_history_rows();

        rows.row_data(0).unwrap()
    }
}

#[test]
fn cancellation_preserves_local_file_and_history_record() {
    let fixture = Fixture::new();

    fixture
        .window
        .invoke_delete_history_local(fixture.row().action_token);
    assert!(fixture.window.get_confirmation_visible());
    assert_eq!(
        fixture.window.get_confirmation_target(),
        fixture.entry.local_path
    );
    fixture.window.invoke_confirmation_response(false);
    fixture.window.invoke_confirmation_response(true);
    assert!(fixture.directory.path().join("capture.png").is_file());
    assert_eq!(
        fixture.storage.history_entry(fixture.entry.id).unwrap(),
        Some(fixture.entry)
    );
}

#[test]
fn accepting_stale_selection_cannot_remove_history_or_local_file() {
    let fixture = Fixture::new();

    fixture
        .window
        .invoke_delete_history_local(fixture.row().action_token);
    fixture
        .window
        .invoke_history_query_changed("different search".into(), 0);
    fixture.window.invoke_confirmation_response(true);
    assert!(fixture.directory.path().join("capture.png").is_file());
    assert!(
        fixture
            .storage
            .history_entry(fixture.entry.id)
            .unwrap()
            .is_some()
    );
    assert!(
        fixture
            .window
            .get_history_action_detail()
            .contains("History changed")
    );
}

#[test]
fn accepting_changed_database_record_preserves_replacement_record_and_file() {
    let fixture = Fixture::new();

    fixture
        .window
        .invoke_remove_history_entry(fixture.row().action_token);
    fixture
        .storage
        .clear_history_local_path(fixture.entry.id)
        .unwrap();
    fixture.window.invoke_confirmation_response(true);
    assert!(
        fixture
            .storage
            .history_entry(fixture.entry.id)
            .unwrap()
            .is_some()
    );
    assert!(fixture.directory.path().join("capture.png").is_file());
    assert!(
        fixture
            .window
            .get_history_action_detail()
            .contains("History changed")
    );
}

#[test]
fn confirmed_local_delete_only_clears_local_copy_and_runs_once() {
    let fixture = Fixture::new();

    fixture
        .window
        .invoke_delete_history_local(fixture.row().action_token);
    fixture.window.invoke_confirmation_response(true);
    fixture.window.invoke_confirmation_response(true);
    wait_until_idle_for_test(&fixture.window, &fixture.controller);
    assert!(!fixture.directory.path().join("capture.png").exists());
    let entry = fixture
        .storage
        .history_entry(fixture.entry.id)
        .unwrap()
        .unwrap();

    assert_eq!(entry.id, fixture.entry.id);
    assert!(entry.local_path.is_empty());
}
