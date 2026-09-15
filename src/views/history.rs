//! Paginated upload-history presentation and row actions.

use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    process::Command,
    rc::Rc,
};

use anyhow::{Context as _, Result};
use cap_std::{ambient_authority, fs::Dir};
use slint::{ComponentHandle as _, Model as _, ModelRc, SharedString, VecModel};

use crate::{AppWindow, HistoryRow};
use sharer::{
    history::{HistoryEntry, HistorySort, HistorySortColumn, SortDirection},
    storage::PlatformStorage,
};

const PAGE_SIZE: usize = 25;

pub(crate) type LocalDeletionObserver = Rc<dyn Fn(&AppWindow, i64, &Path)>;

pub(crate) struct HistoryController {
    local_deleted: LocalDeletionObserver,
    page: usize,
    total: u64,
    rows: Rc<VecModel<HistoryRow>>,
    sort: HistorySort,
    suspended: bool,
}

pub(crate) fn initialize(
    window: &AppWindow,
    storage: &Rc<PlatformStorage>,
    suspended: bool,
    local_deleted: &LocalDeletionObserver,
) -> Result<Rc<RefCell<HistoryController>>> {
    let rows = Rc::new(VecModel::default());
    let controller = Rc::new(RefCell::new(HistoryController {
        local_deleted: Rc::clone(local_deleted),
        page: 0,
        total: 0,
        rows: Rc::clone(&rows),
        sort: HistorySort::default(),
        suspended,
    }));

    window.set_history_rows(ModelRc::from(rows));
    window.set_history_sort_column(HistorySort::default().column.index());
    window.set_history_sort_ascending(HistorySort::default().direction == SortDirection::Ascending);

    if !suspended {
        refresh(window, storage, &controller, 0)?;
    }

    wire_callbacks(window, storage, &controller, local_deleted);

    Ok(controller)
}

pub(crate) fn observe_inserted(
    window: &AppWindow,
    storage: &PlatformStorage,
    controller: &RefCell<HistoryController>,
) -> Result<()> {
    refresh_current(window, storage, controller)
}

pub(crate) fn suspend(controller: &RefCell<HistoryController>) {
    let mut controller = controller.borrow_mut();

    controller.suspended = true;
    controller.rows.set_vec(Vec::new());
}

pub(crate) fn refresh_current(
    window: &AppWindow,
    storage: &PlatformStorage,
    controller: &RefCell<HistoryController>,
) -> Result<()> {
    let page = controller.borrow().page;

    refresh(window, storage, controller, page)
}

pub(crate) fn resume_empty(window: &AppWindow, controller: &RefCell<HistoryController>) {
    let mut controller = controller.borrow_mut();

    controller.suspended = false;
    controller.page = 0;
    controller.total = 0;
    controller.rows.set_vec(Vec::new());
    update_pagination(window, &controller);
}

fn wire_sort_callback(
    window: &AppWindow,
    storage: &Rc<PlatformStorage>,
    controller: &Rc<RefCell<HistoryController>>,
) {
    let window_weak = window.as_weak();
    let storage = Rc::clone(storage);
    let controller = Rc::clone(controller);

    window.on_history_sort_column_clicked(move |column| {
        let Some(column) = HistorySortColumn::from_index(column) else {
            return;
        };
        let sort = next_sort(controller.borrow().sort, column);

        if let Some(window) = window_weak.upgrade()
            && let Err(error) = refresh_with_sort(&window, &storage, &controller, 0, sort)
        {
            window.set_status_detail(format!("Could not sort history: {error:#}").into());
        }
    });
}

fn wire_callbacks(
    window: &AppWindow,
    storage: &Rc<PlatformStorage>,
    controller: &Rc<RefCell<HistoryController>>,
    local_deleted: &LocalDeletionObserver,
) {
    wire_sort_callback(window, storage, controller);
    wire_link_callbacks(window, controller);
    wire_local_file_callbacks(window, controller);
    wire_removal_callbacks(window, storage, controller, local_deleted);
    wire_pagination_callbacks(window, storage, controller);
}

fn wire_link_callbacks(window: &AppWindow, controller: &Rc<RefCell<HistoryController>>) {
    let window_weak = window.as_weak();
    let controller_for_open = Rc::clone(controller);

    window.on_open_history_link(move |index| {
        let Some(row) = row_at(&controller_for_open, index) else {
            return;
        };

        if row.link.is_empty() {
            return;
        }

        if let Err(error) = open::that(row.link.as_str())
            && let Some(window) = window_weak.upgrade()
        {
            window.set_status_detail(format!("Could not open link: {error}").into());
        }
    });

    let window_weak = window.as_weak();
    let controller_for_copy = Rc::clone(controller);

    window.on_copy_history_link(move |index| {
        copy_row_value(&window_weak, &controller_for_copy, index, false);
    });

    let window_weak = window.as_weak();
    let controller_for_delete = Rc::clone(controller);

    window.on_copy_history_delete(move |index| {
        copy_row_value(&window_weak, &controller_for_delete, index, true);
    });
}

fn wire_local_file_callbacks(window: &AppWindow, controller: &Rc<RefCell<HistoryController>>) {
    let window_weak = window.as_weak();
    let controller_for_open_local = Rc::clone(controller);

    window.on_open_history_local(move |index| {
        run_local_action(&window_weak, &controller_for_open_local, index, |path| {
            anyhow::ensure!(path.is_file(), "local capture no longer exists");
            open::that(path).context("failed to open local capture")?;
            Ok("Opened local capture".to_owned())
        });
    });

    let window_weak = window.as_weak();
    let controller_for_reveal = Rc::clone(controller);

    window.on_reveal_history_local(move |index| {
        run_local_action(&window_weak, &controller_for_reveal, index, |path| {
            reveal_file(path)?;
            Ok("Opened capture folder".to_owned())
        });
    });

    let window_weak = window.as_weak();
    let controller_for_copy_file = Rc::clone(controller);

    window.on_copy_history_file(move |index| {
        run_local_action(&window_weak, &controller_for_copy_file, index, |path| {
            sharer::clipboard::copy_file(path)?;
            Ok("Local file copied to clipboard".to_owned())
        });
    });

    let window_weak = window.as_weak();
    let controller_for_copy_path = Rc::clone(controller);

    window.on_copy_history_path(move |index| {
        run_local_action(&window_weak, &controller_for_copy_path, index, |path| {
            sharer::clipboard::copy_link(path.to_string_lossy().as_ref())?;
            Ok("Local path copied to clipboard".to_owned())
        });
    });
}

fn wire_removal_callbacks(
    window: &AppWindow,
    storage: &Rc<PlatformStorage>,
    controller: &Rc<RefCell<HistoryController>>,
    local_deleted: &LocalDeletionObserver,
) {
    let window_weak = window.as_weak();
    let storage_for_remove = Rc::clone(storage);
    let controller_for_remove = Rc::clone(controller);

    window.on_remove_history_entry(move |index| {
        let Some(row) = row_at(&controller_for_remove, index) else {
            return;
        };
        let Some(window) = window_weak.upgrade() else {
            return;
        };

        if !confirm(
            "Remove history entry?",
            "This removes the record from ShareR history. Local and remote files are not deleted.",
        ) {
            return;
        }

        let result = history_id(&row)
            .and_then(|id| storage_for_remove.remove_history(id))
            .and_then(|()| refresh_current(&window, &storage_for_remove, &controller_for_remove));

        set_action_result(&window, result.map(|()| "History entry removed".to_owned()));
    });

    let window_weak = window.as_weak();
    let storage_for_delete_local = Rc::clone(storage);
    let controller_for_delete_local = Rc::clone(controller);
    let local_deleted_for_callback = Rc::clone(local_deleted);

    window.on_delete_history_local(move |index| {
        let Some(row) = row_at(&controller_for_delete_local, index) else {
            return;
        };
        let Some(window) = window_weak.upgrade() else {
            return;
        };

        if !confirm(
            "Delete local capture?",
            "This permanently deletes the local file. Any uploaded copy is unchanged.",
        ) {
            return;
        }

        match delete_local_capture(
            &row,
            DeleteLocalCallbacks {
                mark_deleted: |id, path: &Path| {
                    clear_local_row(&controller_for_delete_local, index);
                    local_deleted_for_callback(&window, id, path);
                },
                clear_persisted: |id| storage_for_delete_local.clear_history_local_path(id),
            },
        ) {
            Ok(None) => window.set_status_detail("Local capture deleted".into()),

            Ok(Some(error)) => window.set_status_detail(
                format!(
                    "Local capture deleted, but its history record could not be updated. \
                     ShareR will reconcile it when this page reloads: {error:#}"
                )
                .into(),
            ),

            Err(error) => set_action_result(&window, Err(error)),
        }
    });
}

fn wire_pagination_callbacks(
    window: &AppWindow,
    storage: &Rc<PlatformStorage>,
    controller: &Rc<RefCell<HistoryController>>,
) {
    let window_weak = window.as_weak();
    let storage_for_previous = Rc::clone(storage);
    let controller_for_previous = Rc::clone(controller);

    window.on_history_previous(move || {
        let page = controller_for_previous.borrow().page.saturating_sub(1);

        if let Some(window) = window_weak.upgrade()
            && let Err(error) = refresh(
                &window,
                &storage_for_previous,
                &controller_for_previous,
                page,
            )
        {
            window.set_status_detail(format!("Could not load history: {error:#}").into());
        }
    });

    let window_weak = window.as_weak();
    let storage_for_next = Rc::clone(storage);
    let controller_for_next = Rc::clone(controller);

    window.on_history_next(move || {
        let page = controller_for_next.borrow().page.saturating_add(1);

        if let Some(window) = window_weak.upgrade()
            && let Err(error) = refresh(&window, &storage_for_next, &controller_for_next, page)
        {
            window.set_status_detail(format!("Could not load history: {error:#}").into());
        }
    });
}

fn refresh(
    window: &AppWindow,
    storage: &PlatformStorage,
    controller: &RefCell<HistoryController>,
    requested_page: usize,
) -> Result<()> {
    let sort = controller.borrow().sort;

    refresh_with_sort(window, storage, controller, requested_page, sort)
}

fn refresh_with_sort(
    window: &AppWindow,
    storage: &PlatformStorage,
    controller: &RefCell<HistoryController>,
    requested_page: usize,
    sort: HistorySort,
) -> Result<()> {
    let total = storage.history_count()?;
    let page_count = page_count(total);
    let page = requested_page.min(page_count.saturating_sub(1));
    let offset = page
        .checked_mul(PAGE_SIZE)
        .context("history page offset overflowed")?;
    let mut entries = storage.history_page_sorted(offset, PAGE_SIZE, sort)?;
    let original_local_paths = entries
        .iter()
        .filter(|entry| !entry.local_path.is_empty())
        .map(|entry| (entry.id, PathBuf::from(&entry.local_path)))
        .collect::<Vec<_>>();
    let reconciliation = storage.reconcile_missing_history_local_paths(&mut entries)?;
    let local_deleted = Rc::clone(&controller.borrow().local_deleted);

    notify_reconciled_local_deletions(&original_local_paths, &entries, |id, path| {
        local_deleted(window, id, path);
    });
    let rows = entries.iter().map(row_from_entry).collect::<Vec<_>>();
    let mut controller = controller.borrow_mut();

    controller.page = page;
    controller.total = total;
    controller.sort = sort;
    controller.suspended = false;
    controller.rows.set_vec(rows);
    window.set_history_sort_column(sort.column.index());
    window.set_history_sort_ascending(sort.direction == SortDirection::Ascending);
    update_pagination(window, &controller);

    if reconciliation.cleared != 0
        || reconciliation.unavailable != 0
        || reconciliation.not_regular != 0
    {
        window.set_status_detail(
            format!(
                "History warning: cleared {} confirmed missing local path(s); kept {} path(s) \
                 that could not be checked and {} path(s) that are not regular files.",
                reconciliation.cleared, reconciliation.unavailable, reconciliation.not_regular
            )
            .into(),
        );
    }

    Ok(())
}

fn notify_reconciled_local_deletions(
    original_local_paths: &[(i64, PathBuf)],
    entries: &[HistoryEntry],
    mut notify: impl FnMut(i64, &Path),
) {
    for (id, path) in original_local_paths {
        if entries
            .iter()
            .any(|entry| entry.id == *id && entry.local_path.is_empty())
        {
            notify(*id, path);
        }
    }
}

fn next_sort(current: HistorySort, column: HistorySortColumn) -> HistorySort {
    let direction = if current.column == column {
        match current.direction {
            SortDirection::Ascending => SortDirection::Descending,
            SortDirection::Descending => SortDirection::Ascending,
        }
    } else if column == HistorySortColumn::CreatedAt {
        SortDirection::Descending
    } else {
        SortDirection::Ascending
    };

    HistorySort { column, direction }
}

fn update_pagination(window: &AppWindow, controller: &HistoryController) {
    let pages = page_count(controller.total);

    window.set_history_page_label(
        format!(
            "Page {} of {pages}  -  {} items",
            controller.page + 1,
            controller.total
        )
        .into(),
    );
    window.set_history_has_previous(controller.page > 0);
    window.set_history_has_next(controller.page + 1 < pages);
}

fn page_count(total: u64) -> usize {
    let total = usize::try_from(total).unwrap_or(usize::MAX);

    total.max(1).div_ceil(PAGE_SIZE)
}

fn row_at(controller: &RefCell<HistoryController>, index: i32) -> Option<HistoryRow> {
    usize::try_from(index)
        .ok()
        .and_then(|index| controller.borrow().rows.row_data(index))
}

fn clear_local_row(controller: &RefCell<HistoryController>, index: i32) {
    let Ok(index) = usize::try_from(index) else {
        return;
    };
    let controller = controller.borrow();
    let Some(mut row) = controller.rows.row_data(index) else {
        return;
    };

    row.local_path = SharedString::new();
    controller.rows.set_row_data(index, row);
}

struct DeleteLocalCallbacks<M, C> {
    mark_deleted: M,
    clear_persisted: C,
}

fn delete_local_capture<M, C>(
    row: &HistoryRow,
    callbacks: DeleteLocalCallbacks<M, C>,
) -> Result<Option<anyhow::Error>>
where
    M: FnOnce(i64, &Path),
    C: FnOnce(i64) -> Result<()>,
{
    let path = local_path(row)?;
    let id = history_id(row)?;

    remove_local_file(&path)?;
    (callbacks.mark_deleted)(id, &path);

    Ok((callbacks.clear_persisted)(id).err())
}

fn remove_local_file(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .context("local capture has no parent directory")?;
    let filename = path.file_name().context("local capture has no filename")?;
    let directory = Dir::open_ambient_dir(parent, ambient_authority())
        .with_context(|| format!("failed to open {}", parent.display()))?;

    directory
        .remove_file(filename)
        .with_context(|| format!("failed to delete {}", path.display()))
}

fn copy_row_value(
    window: &slint::Weak<AppWindow>,
    controller: &RefCell<HistoryController>,
    index: i32,
    delete_link: bool,
) {
    let Some(row) = row_at(controller, index) else {
        return;
    };
    let (value, description) = if delete_link {
        (row.delete_link, "Deletion URL")
    } else {
        (row.link, "Public link")
    };

    if value.is_empty() {
        return;
    }

    let result = sharer::clipboard::copy_link(value.as_str());

    if let Some(window) = window.upgrade() {
        let detail = match result {
            Ok(()) => SharedString::from(format!("{description} copied to clipboard")),
            Err(error) => SharedString::from(format!("Could not copy {description}: {error:#}")),
        };

        window.set_status_detail(detail);
    }
}

fn row_from_entry(entry: &HistoryEntry) -> HistoryRow {
    HistoryRow {
        entry_id: entry.id.to_string().into(),
        created: format_created_at(entry.created_at).into(),
        filename: entry.filename.as_str().into(),
        size: format_size(entry.size_bytes).into(),
        expires: if entry.expires_at.is_empty() {
            "Local only".into()
        } else {
            entry.expires_at.as_str().into()
        },
        link: entry.link.as_str().into(),
        delete_link: entry.delete_url.as_str().into(),
        local_path: entry.local_path.as_str().into(),
    }
}

fn format_created_at(timestamp: i64) -> String {
    if timestamp <= 0 {
        return "Unknown".to_owned();
    }

    chrono::DateTime::from_timestamp(timestamp, 0).map_or_else(
        || "Unknown".to_owned(),
        |created| {
            created
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        },
    )
}

fn run_local_action(
    window: &slint::Weak<AppWindow>,
    controller: &RefCell<HistoryController>,
    index: i32,
    action: impl FnOnce(&Path) -> Result<String>,
) {
    let Some(row) = row_at(controller, index) else {
        return;
    };
    let result = local_path(&row).and_then(|path| action(&path));

    if let Some(window) = window.upgrade() {
        set_action_result(&window, result);
    }
}

fn local_path(row: &HistoryRow) -> Result<PathBuf> {
    anyhow::ensure!(!row.local_path.is_empty(), "this entry has no local file");
    Ok(PathBuf::from(row.local_path.as_str()))
}

fn history_id(row: &HistoryRow) -> Result<i64> {
    row.entry_id
        .parse()
        .context("history entry has an invalid identifier")
}

fn set_action_result(window: &AppWindow, result: Result<String>) {
    match result {
        Ok(detail) => window.set_status_detail(detail.into()),
        Err(error) => window.set_status_detail(format!("History action failed: {error:#}").into()),
    }
}

fn confirm(title: &str, description: &str) -> bool {
    rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Warning)
        .set_title(title)
        .set_description(description)
        .set_buttons(rfd::MessageButtons::YesNo)
        .show()
        == rfd::MessageDialogResult::Yes
}

#[cfg(windows)]
pub(crate) fn reveal_file(path: &Path) -> Result<()> {
    anyhow::ensure!(path.exists(), "local capture no longer exists");
    Command::new("explorer.exe")
        .arg(format!("/select,{}", path.display()))
        .spawn()
        .context("failed to open File Explorer")?;
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn reveal_file(path: &Path) -> Result<()> {
    anyhow::ensure!(path.exists(), "local capture no longer exists");
    Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn()
        .context("failed to open Finder")?;
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn reveal_file(path: &Path) -> Result<()> {
    anyhow::ensure!(path.exists(), "local capture no longer exists");
    let parent = path
        .parent()
        .context("local capture has no parent folder")?;

    open::that(parent).context("failed to open capture folder")?;
    Ok(())
}

fn format_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;

    if bytes >= GIB {
        format_decimal_size(bytes, GIB, "GiB")
    } else if bytes >= MIB {
        format_decimal_size(bytes, MIB, "MiB")
    } else if bytes >= KIB {
        format_decimal_size(bytes, KIB, "KiB")
    } else {
        format!("{bytes} B")
    }
}

fn format_decimal_size(bytes: u64, unit: u64, suffix: &str) -> String {
    let whole = bytes / unit;
    let decimal = (bytes % unit) * 10 / unit;

    format!("{whole}.{decimal} {suffix}")
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use sharer::history::{HistorySort, HistorySortColumn, SortDirection};
    use slint::SharedString;

    use crate::HistoryRow;

    use super::{
        DeleteLocalCallbacks, delete_local_capture, format_created_at, format_size, next_sort,
        notify_reconciled_local_deletions, page_count,
    };

    #[test]
    fn sizes_are_compact_and_readable() {
        assert_eq!(format_size(900), "900 B");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MiB");
    }

    #[test]
    fn page_count_always_has_a_visible_first_page() {
        assert_eq!(page_count(0), 1);
        assert_eq!(page_count(25), 1);
        assert_eq!(page_count(26), 2);
    }

    #[test]
    fn missing_creation_dates_are_labeled_honestly() {
        assert_eq!(format_created_at(0), "Unknown");
        assert_eq!(format_created_at(i64::MAX), "Unknown");
    }

    #[test]
    fn clicking_headers_selects_and_toggles_each_sort() {
        let default = HistorySort::default();

        assert_eq!(default.column, HistorySortColumn::CreatedAt);
        assert_eq!(default.direction, SortDirection::Descending);

        let filename = next_sort(default, HistorySortColumn::Filename);

        assert_eq!(filename.direction, SortDirection::Ascending);
        assert_eq!(
            next_sort(filename, HistorySortColumn::Filename).direction,
            SortDirection::Descending
        );

        let date = next_sort(filename, HistorySortColumn::CreatedAt);

        assert_eq!(date.direction, SortDirection::Descending);
    }

    #[test]
    fn deleted_file_is_authoritative_when_database_cleanup_fails() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("capture.png");

        std::fs::write(&path, b"capture").unwrap();
        let row = HistoryRow {
            entry_id: "42".into(),
            local_path: SharedString::from(path.to_string_lossy().as_ref()),
            ..Default::default()
        };
        let local_actions_cleared = Cell::new(false);

        let cleanup_error = delete_local_capture(
            &row,
            DeleteLocalCallbacks {
                mark_deleted: |id, deleted_path: &std::path::Path| {
                    assert_eq!(id, 42);
                    assert_eq!(deleted_path, path);
                    local_actions_cleared.set(true);
                },
                clear_persisted: |_| anyhow::bail!("injected database cleanup failure"),
            },
        )
        .unwrap()
        .expect("cleanup failure should request reconciliation");

        assert!(!path.exists());
        assert!(local_actions_cleared.get());
        assert!(cleanup_error.to_string().contains("injected database"));
    }

    #[test]
    fn failed_file_deletion_keeps_local_actions_and_skips_database_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing.png");
        let row = HistoryRow {
            entry_id: "42".into(),
            local_path: SharedString::from(path.to_string_lossy().as_ref()),
            ..Default::default()
        };
        let local_actions_cleared = Cell::new(false);
        let cleanup_attempted = Cell::new(false);

        assert!(
            delete_local_capture(
                &row,
                DeleteLocalCallbacks {
                    mark_deleted: |_, _: &std::path::Path| local_actions_cleared.set(true),
                    clear_persisted: |_| {
                        cleanup_attempted.set(true);
                        Ok(())
                    },
                },
            )
            .is_err()
        );
        assert!(!local_actions_cleared.get());
        assert!(!cleanup_attempted.get());
    }

    #[test]
    fn reconciliation_notifies_only_confirmed_cleared_paths() {
        let missing = std::path::PathBuf::from("missing.png");
        let unavailable = std::path::PathBuf::from("unavailable.png");
        let not_regular = std::path::PathBuf::from("directory");
        let original = vec![
            (1, missing.clone()),
            (2, unavailable.clone()),
            (3, not_regular.clone()),
        ];
        let entries = vec![
            sharer::history::HistoryEntry {
                id: 1,
                local_path: String::new(),
                filename: "missing.png".to_owned(),
                link: String::new(),
                delete_url: String::new(),
                expires_at: String::new(),
                created_at: 1,
                size_bytes: 1,
            },
            sharer::history::HistoryEntry {
                id: 2,
                local_path: unavailable.to_string_lossy().into_owned(),
                filename: "unavailable.png".to_owned(),
                link: String::new(),
                delete_url: String::new(),
                expires_at: String::new(),
                created_at: 2,
                size_bytes: 2,
            },
            sharer::history::HistoryEntry {
                id: 3,
                local_path: not_regular.to_string_lossy().into_owned(),
                filename: "directory".to_owned(),
                link: String::new(),
                delete_url: String::new(),
                expires_at: String::new(),
                created_at: 3,
                size_bytes: 3,
            },
        ];
        let notified = RefCell::new(Vec::new());

        notify_reconciled_local_deletions(&original, &entries, |id, path| {
            notified.borrow_mut().push((id, path.to_owned()));
        });

        assert_eq!(*notified.borrow(), vec![(1, missing)]);
    }
}
