//! Paginated upload-history presentation and row actions.

#[cfg(any(windows, target_os = "macos"))]
use std::process::Command;
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};

use anyhow::{Context as _, Result};
use cap_std::{ambient_authority, fs::Dir};
use slint::{ComponentHandle as _, Model as _, ModelRc, SharedString, VecModel};

use crate::{AppWindow, HistoryRow};
use sharer::{
    history::{HistoryEntry, HistoryFilter, HistorySort, HistorySortColumn, SortDirection},
    presentation::safe_text,
    storage::{HistoryLocalPathReconciliation, PlatformStorage},
};

const PAGE_SIZE: usize = 25;

mod worker;
pub(crate) use worker::HistoryLoader;
use worker::{HistoryEvent, Reply};

pub(crate) type LocalDeletionObserver = Rc<dyn Fn(&AppWindow, i64, &Path)>;
pub(crate) type HistoryRemovalObserver = Rc<dyn Fn(&AppWindow, i64)>;

#[derive(Clone, Default)]
struct HistorySearch {
    query: String,
    filter: HistoryFilter,
}

pub(crate) struct HistoryController {
    local_deleted: LocalDeletionObserver,
    loader: Rc<RefCell<HistoryLoader>>,
    poll_timer: slint::Timer,
    generation: u64,
    page: usize,
    total: u64,
    rows: Rc<VecModel<HistoryRow>>,
    sort: HistorySort,
    search: HistorySearch,
    suspended: bool,
    checking: bool,
    loaded: bool,
}

#[cfg(test)]
pub(crate) fn initialize(
    window: &AppWindow,
    storage: &Rc<PlatformStorage>,
    suspended: bool,
    local_deleted: &LocalDeletionObserver,
    history_removed: &HistoryRemovalObserver,
) -> Result<Rc<RefCell<HistoryController>>> {
    let loader = Rc::new(RefCell::new(HistoryLoader::new(storage)?));
    let controller = initialize_with_loader(
        window,
        storage,
        suspended,
        local_deleted,
        history_removed,
        &loader,
    )?;

    if !suspended {
        wait_until_idle_for_test(window, &controller);
    }

    Ok(controller)
}

pub(crate) fn initialize_with_loader(
    window: &AppWindow,
    storage: &Rc<PlatformStorage>,
    suspended: bool,
    local_deleted: &LocalDeletionObserver,
    history_removed: &HistoryRemovalObserver,
    loader: &Rc<RefCell<HistoryLoader>>,
) -> Result<Rc<RefCell<HistoryController>>> {
    let rows = Rc::new(VecModel::default());
    let controller = Rc::new(RefCell::new(HistoryController {
        local_deleted: Rc::clone(local_deleted),
        loader: Rc::clone(loader),
        poll_timer: slint::Timer::default(),
        generation: loader.borrow_mut().cancel()?,
        page: 0,
        total: 0,
        rows: Rc::clone(&rows),
        sort: HistorySort::default(),
        search: HistorySearch::default(),
        suspended,
        checking: false,
        loaded: false,
    }));

    window.set_history_rows(ModelRc::from(rows));
    window.set_history_sort_column(HistorySort::default().column.index());
    window.set_history_sort_ascending(HistorySort::default().direction == SortDirection::Ascending);
    window.set_history_checking(false);
    window.set_history_checking_detail("".into());
    window.set_history_load_message("".into());

    let weak_window = window.as_weak();
    let weak_controller = Rc::downgrade(&controller);

    controller.borrow().poll_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(16),
        move || {
            let (Some(window), Some(controller)) =
                (weak_window.upgrade(), weak_controller.upgrade())
            else {
                return;
            };
            // Bound each UI turn even if the worker has finished several batches.

            for _ in 0..4 {
                let (loader, generation) = {
                    let controller = controller.borrow();

                    (Rc::clone(&controller.loader), controller.generation)
                };

                if !loader.borrow().is_current(generation) {
                    break;
                }

                let Some(reply) = loader.borrow_mut().poll() else {
                    break;
                };

                apply_reply(&window, &controller, reply);
            }
        },
    );

    if !suspended {
        refresh(window, storage, &controller, 0)?;
    }

    wire_callbacks(window, storage, &controller, local_deleted, history_removed);

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

    let cancellation = controller.loader.borrow_mut().cancel();

    if let Ok(generation) = cancellation {
        controller.generation = generation;
    }

    controller.suspended = true;
    controller.checking = false;
    controller.loaded = false;
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
    suspend(controller);
    let mut controller = controller.borrow_mut();

    controller.suspended = false;
    controller.page = 0;
    controller.total = 0;
    controller.rows.set_vec(Vec::new());
    window.set_history_checking(false);
    window.set_history_checking_detail("".into());
    window.set_history_load_message("".into());
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
            set_action_status(&window, format!("Could not sort history: {error:#}"), true);
        }
    });
}

fn wire_search_callback(
    window: &AppWindow,
    storage: &Rc<PlatformStorage>,
    controller: &Rc<RefCell<HistoryController>>,
) {
    let window_weak = window.as_weak();
    let storage = Rc::clone(storage);
    let controller = Rc::clone(controller);

    window.on_history_query_changed(move |query, filter| {
        let Some(filter) = HistoryFilter::from_index(filter) else {
            return;
        };
        let Some(window) = window_weak.upgrade() else {
            return;
        };
        let search = HistorySearch {
            query: query.to_string(),
            filter,
        };
        let sort = controller.borrow().sort;

        {
            let controller = controller.borrow();

            if controller.search.query == search.query
                && controller.search.filter == search.filter
                && (controller.loaded || controller.checking)
            {
                return;
            }
        }

        let requested_query = search.query.clone();

        if let Err(error) = refresh_selection(&window, &storage, &controller, 0, sort, search) {
            let controller = controller.borrow();

            window.set_history_search_query(controller.search.query.as_str().into());
            window.set_history_filter(controller.search.filter.index());
            set_action_status(
                &window,
                format!("Could not search history: {error:#}"),
                true,
            );
        } else {
            window.set_history_search_query(requested_query.into());
            window.set_history_filter(filter.index());
        }
    });
}

fn wire_callbacks(
    window: &AppWindow,
    storage: &Rc<PlatformStorage>,
    controller: &Rc<RefCell<HistoryController>>,
    local_deleted: &LocalDeletionObserver,
    history_removed: &HistoryRemovalObserver,
) {
    wire_sort_callback(window, storage, controller);
    wire_search_callback(window, storage, controller);
    wire_link_callbacks(window, storage, controller);
    wire_local_file_callbacks(window, controller);
    wire_removal_callbacks(window, storage, controller, local_deleted, history_removed);
    wire_pagination_callbacks(window, storage, controller);
    wire_cancel_callback(window, controller);
}

fn wire_cancel_callback(window: &AppWindow, controller: &Rc<RefCell<HistoryController>>) {
    let weak_window = window.as_weak();
    let controller = Rc::clone(controller);

    window.on_history_cancel_check(move || {
        let Some(window) = weak_window.upgrade() else {
            return;
        };
        let loader = Rc::clone(&controller.borrow().loader);

        match loader.borrow_mut().cancel() {
            Ok(generation) => {
                let mut controller = controller.borrow_mut();

                controller.generation = generation;
                controller.checking = false;
                controller.loaded = false;
                controller.rows.set_vec(Vec::new());
                window.set_history_checking(false);
                window.set_history_load_message("Search cancelled".into());
                window.set_history_checking_detail(
                    "Your history is kept. Retry or change the search.".into(),
                );
                window.set_history_page_label("Search cancelled".into());
                window.set_history_has_previous(false);
                window.set_history_has_next(false);
            }

            Err(error) => set_action_status(
                &window,
                format!("Could not cancel history search: {error:#}"),
                true,
            ),
        }
    });
}

fn wire_link_callbacks(
    window: &AppWindow,
    storage: &Rc<PlatformStorage>,
    controller: &Rc<RefCell<HistoryController>>,
) {
    let window_weak = window.as_weak();
    let controller_for_open = Rc::clone(controller);

    window.on_open_history_link(move |action_token| {
        let Some(row) = row_by_token(&controller_for_open, action_token.as_str()) else {
            return;
        };

        if row.link.is_empty() {
            return;
        }

        if let Err(error) = open::that(row.link.as_str())
            && let Some(window) = window_weak.upgrade()
        {
            set_action_status(&window, format!("Could not open link: {error}"), true);
        }
    });

    let window_weak = window.as_weak();
    let controller_for_copy = Rc::clone(controller);

    window.on_copy_history_link(move |action_token| {
        copy_row_value(
            &window_weak,
            &controller_for_copy,
            action_token.as_str(),
            false,
        );
    });

    let window_weak = window.as_weak();
    let controller_for_delete = Rc::clone(controller);

    window.on_copy_history_delete(move |action_token| {
        copy_row_value(
            &window_weak,
            &controller_for_delete,
            action_token.as_str(),
            true,
        );
    });

    let window_weak = window.as_weak();
    let controller_for_open_delete = Rc::clone(controller);
    let storage_for_open_delete = Rc::clone(storage);

    window.on_open_history_delete(move |action_token| {
        let Some(window) = window_weak.upgrade() else {
            return;
        };
        let Some(row) = row_by_token(&controller_for_open_delete, action_token.as_str()) else {
            return;
        };

        if row.delete_link.is_empty() {
            return;
        }

        let entry =
            match confirmation_entry(&storage_for_open_delete, &controller_for_open_delete, &row) {
                Ok(entry) => entry,

                Err(error) => {
                    set_action_result(&window, Err(error));

                    return;
                }
            };
        let storage = Rc::clone(&storage_for_open_delete);
        let controller = Rc::clone(&controller_for_open_delete);
        let target = entry.filename.clone();

        super::confirmation::deletion_page(&window, &target, move |window| {
            if let Err(error) = confirm_unchanged_entry(&storage, &controller, &row, &entry) {
                set_action_result(window, Err(error));

                return;
            }

            let result = open::that(entry.delete_url.as_str());

            set_action_status(window, deletion_page_status(&result), result.is_err());
        });
    });
}

pub(crate) fn deletion_page_status(result: &std::io::Result<()>) -> &'static str {
    if result.is_ok() {
        "Opened deletion page in the browser"
    } else {
        // Launcher errors can contain command arguments, including the secret capability URL.
        "Could not open the deletion page. Copy its link and open it in your browser."
    }
}

fn wire_local_file_callbacks(window: &AppWindow, controller: &Rc<RefCell<HistoryController>>) {
    let window_weak = window.as_weak();
    let controller_for_open_local = Rc::clone(controller);

    window.on_open_history_local(move |action_token| {
        run_local_action(
            &window_weak,
            &controller_for_open_local,
            action_token.as_str(),
            |path| {
                anyhow::ensure!(path.is_file(), "local capture no longer exists");
                open::that(path).context("failed to open local capture")?;
                Ok("Opened local capture".to_owned())
            },
        );
    });

    let window_weak = window.as_weak();
    let controller_for_reveal = Rc::clone(controller);

    window.on_reveal_history_local(move |action_token| {
        run_local_action(
            &window_weak,
            &controller_for_reveal,
            action_token.as_str(),
            |path| {
                reveal_file(path)?;
                Ok("Opened capture folder".to_owned())
            },
        );
    });

    let window_weak = window.as_weak();
    let controller_for_copy_file = Rc::clone(controller);

    window.on_copy_history_file(move |action_token| {
        run_local_action(
            &window_weak,
            &controller_for_copy_file,
            action_token.as_str(),
            |path| {
                sharer::clipboard::copy_file(path)?;
                Ok("Local file copied to clipboard".to_owned())
            },
        );
    });

    let window_weak = window.as_weak();
    let controller_for_copy_path = Rc::clone(controller);

    window.on_copy_history_path(move |action_token| {
        run_local_action(
            &window_weak,
            &controller_for_copy_path,
            action_token.as_str(),
            |path| {
                sharer::clipboard::copy_link(path.to_string_lossy().as_ref())?;
                Ok("Local path copied to clipboard".to_owned())
            },
        );
    });
}

fn wire_removal_callbacks(
    window: &AppWindow,
    storage: &Rc<PlatformStorage>,
    controller: &Rc<RefCell<HistoryController>>,
    local_deleted: &LocalDeletionObserver,
    history_removed: &HistoryRemovalObserver,
) {
    let window_weak = window.as_weak();
    let storage_for_remove = Rc::clone(storage);
    let controller_for_remove = Rc::clone(controller);
    let history_removed = Rc::clone(history_removed);

    window.on_remove_history_entry(move |action_token| {
        let Some(row) = row_by_token(&controller_for_remove, action_token.as_str()) else {
            return;
        };
        let Some(window) = window_weak.upgrade() else {
            return;
        };

        let entry = match confirmation_entry(&storage_for_remove, &controller_for_remove, &row) {
            Ok(entry) => entry,

            Err(error) => {
                set_action_result(&window, Err(error));

                return;
            }
        };
        let storage = Rc::clone(&storage_for_remove);
        let controller = Rc::clone(&controller_for_remove);
        let history_removed = Rc::clone(&history_removed);
        let target = entry.filename.clone();

        super::confirmation::request(
            &window,
            &super::confirmation::Confirmation {
                title: "Remove history entry?",
                description: "This removes the record and its saved links from ShareR history, including the deletion link. Local and uploaded files are not deleted. Copy the deletion link first if you may need it later.",
                target: &target,
                action_label: "Remove from history",
            },
            move |window| {
                let result = confirm_unchanged_entry(&storage, &controller, &row, &entry).and_then(|()| {
                    storage.remove_history(entry.id)?;
                    history_removed(window, entry.id);
                    refresh_current(window, &storage, &controller)
                });

                set_action_result(window, result.map(|()| "History entry removed".to_owned()));
            },
        );
    });

    let window_weak = window.as_weak();
    let storage_for_delete_local = Rc::clone(storage);
    let controller_for_delete_local = Rc::clone(controller);
    let local_deleted_for_callback = Rc::clone(local_deleted);

    window.on_delete_history_local(move |action_token| {
        let Some(row) = row_by_token(&controller_for_delete_local, action_token.as_str()) else {
            return;
        };
        let Some(window) = window_weak.upgrade() else {
            return;
        };

        if row.local_path.is_empty() {
            return;
        }

        let entry = match confirmation_entry(&storage_for_delete_local, &controller_for_delete_local, &row) {
            Ok(entry) => entry,

            Err(error) => {
                set_action_result(&window, Err(error));

                return;
            }
        };
        let storage = Rc::clone(&storage_for_delete_local);
        let controller = Rc::clone(&controller_for_delete_local);
        let local_deleted = Rc::clone(&local_deleted_for_callback);
        let target = entry.local_path.clone();

        super::confirmation::request(
            &window,
            &super::confirmation::Confirmation {
                title: "Delete local file permanently?",
                description: "This permanently deletes the local file from disk, without using the Recycle Bin or Trash. The uploaded copy and history entry stay.",
                target: &target,
                action_label: "Delete permanently",
            },
            move |window| {
                if let Err(error) = confirm_unchanged_entry(&storage, &controller, &row, &entry) {
                    set_action_result(window, Err(error));

                    return;
                }

                finish_local_deletion(window, &storage, &controller, &local_deleted, &row);
            },
        );
    });
}

fn finish_local_deletion(
    window: &AppWindow,
    storage: &PlatformStorage,
    controller: &RefCell<HistoryController>,
    local_deleted: &LocalDeletionObserver,
    row: &HistoryRow,
) {
    match delete_local_capture(
        row,
        DeleteLocalCallbacks {
            mark_deleted: |id: i64, path: &Path| {
                clear_local_row(controller, &id.to_string());
                local_deleted(window, id, path);
            },
            clear_persisted: |id| storage.clear_history_local_path(id),
        },
    ) {
        Ok(None) => {
            let result = refresh_current(window, storage, controller)
                .map(|()| "Local capture deleted".to_owned())
                .context("Local capture deleted, but history could not be refreshed");

            set_action_result(window, result);
        }

        Ok(Some(error)) => set_action_status(
            window,
            format!(
                "Local capture deleted, but its history record could not be updated. \
                 ShareR will reconcile it when this page reloads: {error:#}"
            ),
            true,
        ),

        Err(error) => set_action_result(window, Err(error)),
    }
}

fn confirmation_entry(
    storage: &PlatformStorage,
    controller: &RefCell<HistoryController>,
    expected_row: &HistoryRow,
) -> Result<HistoryEntry> {
    let row = row_by_token(controller, expected_row.action_token.as_str())
        .context("History changed; select the item again")?;
    let entry = storage
        .history_entry(history_id(&row)?)?
        .context("History changed; select the item again")?;

    anyhow::ensure!(
        entry.local_path == expected_row.local_path.as_str()
            && entry.link == expected_row.link.as_str()
            && entry.delete_url == expected_row.delete_link.as_str()
            && safe_text(&entry.filename).as_ref() == expected_row.filename.as_str(),
        "History changed; select the item again"
    );

    Ok(entry)
}

fn confirm_unchanged_entry(
    storage: &PlatformStorage,
    controller: &RefCell<HistoryController>,
    row: &HistoryRow,
    expected: &HistoryEntry,
) -> Result<()> {
    anyhow::ensure!(
        confirmation_entry(storage, controller, row)? == *expected,
        "History changed; select the item again"
    );

    Ok(())
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
        if controller_for_previous.borrow().checking {
            return;
        }

        let page = controller_for_previous.borrow().page.saturating_sub(1);

        if let Some(window) = window_weak.upgrade()
            && let Err(error) = refresh(
                &window,
                &storage_for_previous,
                &controller_for_previous,
                page,
            )
        {
            set_action_status(&window, format!("Could not load history: {error:#}"), true);
        }
    });

    let window_weak = window.as_weak();
    let storage_for_next = Rc::clone(storage);
    let controller_for_next = Rc::clone(controller);

    window.on_history_next(move || {
        if controller_for_next.borrow().checking {
            return;
        }

        let page = controller_for_next.borrow().page.saturating_add(1);

        if let Some(window) = window_weak.upgrade()
            && let Err(error) = refresh(&window, &storage_for_next, &controller_for_next, page)
        {
            set_action_status(&window, format!("Could not load history: {error:#}"), true);
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
    let search = controller.borrow().search.clone();

    refresh_selection(window, storage, controller, requested_page, sort, search)
}

fn refresh_selection(
    window: &AppWindow,
    _storage: &PlatformStorage,
    controller: &RefCell<HistoryController>,
    requested_page: usize,
    sort: HistorySort,
    search: HistorySearch,
) -> Result<()> {
    let loader = Rc::clone(&controller.borrow().loader);
    let generation =
        loader
            .borrow_mut()
            .submit(requested_page, sort, search.query.clone(), search.filter)?;
    let mut controller = controller.borrow_mut();

    controller.generation = generation;
    controller.page = requested_page;
    controller.sort = sort;
    controller.search = search;
    controller.suspended = false;
    controller.checking = true;
    controller.loaded = false;
    // Drop every old action token immediately, before any asynchronous result arrives.
    controller.rows.set_vec(Vec::new());
    window.set_history_sort_column(sort.column.index());
    window.set_history_sort_ascending(sort.direction == SortDirection::Ascending);
    window.set_history_checking(true);
    window.set_history_checking_detail("You can change the search or keep using ShareR.".into());
    window.set_history_load_message("".into());
    window.set_history_page_label("Checking history…".into());
    window.set_history_has_previous(false);
    window.set_history_has_next(false);

    Ok(())
}

fn history_search_matches(entry: &HistoryEntry, query: &str) -> bool {
    let query = query.trim().as_bytes();

    query.is_empty()
        || [&entry.filename, &entry.link, &entry.local_path]
            .into_iter()
            .any(|value| {
                value
                    .as_bytes()
                    .windows(query.len())
                    .any(|part| part.eq_ignore_ascii_case(query))
            })
}

fn apply_reply(window: &AppWindow, controller: &RefCell<HistoryController>, reply: Reply) {
    match reply.event {
        HistoryEvent::Progress { cleared, checked } => {
            let observer = Rc::clone(&controller.borrow().local_deleted);

            // These notifications describe committed changes even after cancellation.
            for (id, path) in cleared {
                observer(window, id, &path);
            }

            let controller = controller.borrow();

            if reply.generation == controller.generation && controller.checking {
                window.set_history_checking_detail(
                    format!(
                        "Checked {checked} history items. You can change the search or cancel."
                    )
                    .into(),
                );
            }
        }

        HistoryEvent::Complete(result) => {
            let mut controller = controller.borrow_mut();

            if reply.generation != controller.generation
                || !controller.checking
                || controller.suspended
            {
                return;
            }

            controller.checking = false;
            window.set_history_checking(false);

            match result {
                Ok(page) => {
                    controller.page = page.page;
                    controller.total = page.total;
                    controller.loaded = true;
                    controller.rows.set_vec(
                        page.entries
                            .iter()
                            .map(|entry| row_from_entry(entry, reply.generation))
                            .collect::<Vec<_>>(),
                    );
                    window.set_history_load_message("".into());
                    window.set_history_checking_detail("".into());
                    update_pagination(window, &controller);
                    report_reconciliation(window, page.reconciliation);
                }

                Err(error) => {
                    controller.loaded = false;
                    controller.rows.set_vec(Vec::new());
                    window.set_history_load_message("Could not load history".into());
                    window.set_history_checking_detail(
                        "Your history is kept. Retry the search.".into(),
                    );
                    window.set_history_page_label("History unavailable".into());
                    set_action_status(window, format!("Could not load history: {error:#}"), true);
                }
            }
        }
    }
}

fn report_reconciliation(window: &AppWindow, reconciliation: HistoryLocalPathReconciliation) {
    if reconciliation.cleared != 0
        || reconciliation.unavailable != 0
        || reconciliation.not_regular != 0
    {
        set_action_status(
            window,
            format!(
                "History warning: cleared {} confirmed missing local path(s); kept {} path(s) \
                 that could not be checked and {} path(s) that are not regular files.",
                reconciliation.cleared, reconciliation.unavailable, reconciliation.not_regular
            ),
            reconciliation.unavailable != 0 || reconciliation.not_regular != 0,
        );
    }
}

#[cfg(test)]
pub(crate) fn wait_until_idle_for_test(
    window: &AppWindow,
    controller: &RefCell<HistoryController>,
) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);

    while controller.borrow().checking {
        let loader = Rc::clone(&controller.borrow().loader);
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());

        assert!(
            !remaining.is_zero(),
            "history presentation did not complete"
        );
        let reply = loader
            .borrow_mut()
            .wait_for_reply(remaining)
            .expect("history worker must complete the test request");

        apply_reply(window, controller, reply);
    }
}

#[cfg(test)]
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
    let unfiltered =
        controller.search.query.trim().is_empty() && controller.search.filter == HistoryFilter::All;
    let noun = match (unfiltered, controller.total == 1) {
        (true, true) => "item",
        (true, false) => "items",
        (false, true) => "match",
        (false, false) => "matches",
    };

    window.set_history_page_label(
        format!(
            "{} {noun} · Page {} of {pages}",
            controller.total,
            controller.page + 1
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

fn row_by_token(controller: &RefCell<HistoryController>, action_token: &str) -> Option<HistoryRow> {
    controller
        .borrow()
        .rows
        .iter()
        .find(|row| row.action_token == action_token)
}

pub(crate) fn local_upload_entry(
    storage: &PlatformStorage,
    controller: &RefCell<HistoryController>,
    action_token: &str,
) -> Result<HistoryEntry> {
    let row = row_by_token(controller, action_token)
        .context("history changed; select the local file again")?;

    anyhow::ensure!(
        row.link.is_empty(),
        "this entry already has an uploaded link"
    );
    anyhow::ensure!(!row.local_path.is_empty(), "this entry has no local file");
    let entry = storage
        .history_entry(history_id(&row)?)?
        .context("history entry no longer exists")?;

    anyhow::ensure!(
        entry.link.is_empty() && entry.local_path == row.local_path.as_str(),
        "history entry changed; select the local file again"
    );
    anyhow::ensure!(
        Path::new(&entry.local_path).is_file(),
        "local capture no longer exists or is not a file"
    );

    Ok(entry)
}

fn clear_local_row(controller: &RefCell<HistoryController>, entry_id: &str) {
    let controller = controller.borrow();
    let Some(index) = controller
        .rows
        .iter()
        .position(|row| row.entry_id == entry_id)
    else {
        return;
    };
    let Some(mut row) = controller.rows.row_data(index) else {
        return;
    };

    row.local_path = SharedString::new();
    row.display_local_path = SharedString::new();
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
    action_token: &str,
    delete_link: bool,
) {
    let Some(row) = row_by_token(controller, action_token) else {
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
        let detail = match &result {
            Ok(()) => format!("{description} copied to clipboard"),
            Err(error) => format!("Could not copy {description}: {error:#}"),
        };

        set_action_status(&window, detail, result.is_err());
    }
}

fn row_from_entry(entry: &HistoryEntry, generation: u64) -> HistoryRow {
    HistoryRow {
        action_token: format!("{generation}:{}", entry.id).into(),
        entry_id: entry.id.to_string().into(),
        created: format_created_at(entry.created_at).into(),
        filename: safe_text(&entry.filename).as_ref().into(),
        size: format_size(entry.size_bytes).into(),
        expires: if entry.expires_at.is_empty() {
            if entry.link.is_empty() {
                "—".into()
            } else {
                "Retention set by host".into()
            }
        } else {
            safe_text(&entry.expires_at).as_ref().into()
        },
        display_link: safe_text(&entry.link).as_ref().into(),
        display_local_path: safe_text(&entry.local_path).as_ref().into(),
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
    action_token: &str,
    action: impl FnOnce(&Path) -> Result<String>,
) {
    let Some(row) = row_by_token(controller, action_token) else {
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
        Ok(detail) => set_action_status(window, detail, false),
        Err(error) => set_action_status(window, format!("{error:#}"), true),
    }
}

pub(crate) fn set_action_status(window: &AppWindow, detail: impl Into<SharedString>, failed: bool) {
    window.set_history_action_detail(detail.into());
    window.set_history_action_failed(failed);
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
        DeleteLocalCallbacks, delete_local_capture, deletion_page_status, format_created_at,
        format_size, next_sort, notify_reconciled_local_deletions, page_count, row_from_entry,
    };

    #[test]
    fn deletion_launch_failure_never_exposes_capability_or_provider_credentials() {
        let capability =
            "https://delete.example/capture?token=fake-delete-token&key=fake-provider-key";
        let error =
            std::io::Error::other(format!("browser command failed with argument {capability}"));
        let detail = deletion_page_status(&Err(error));

        assert!(!detail.is_empty());
        assert_ne!(detail, deletion_page_status(&Ok(())));
        assert!(!detail.contains(capability));
        assert!(!detail.contains("fake-delete-token"));
        assert!(!detail.contains("fake-provider-key"));
    }

    #[test]
    fn sizes_are_compact_and_readable() {
        assert_eq!(format_size(900), "900 B");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MiB");
    }

    #[test]
    fn uploaded_entries_without_an_expiry_show_the_host_retention() {
        let uploaded = sharer::history::HistoryEntry {
            id: 1,
            filename: "capture.png".to_owned(),
            size_bytes: 1,
            link: "https://files.example/capture.png".to_owned(),
            delete_url: String::new(),
            expires_at: String::new(),
            local_path: String::new(),
            created_at: 1,
        };
        let local = sharer::history::HistoryEntry {
            link: String::new(),
            ..uploaded.clone()
        };

        assert_eq!(
            row_from_entry(&uploaded, 0).expires,
            "Retention set by host"
        );
        assert_eq!(row_from_entry(&local, 0).expires, "—");
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

#[cfg(test)]
mod confirmation_tests;
#[cfg(test)]
mod presentation_tests;
#[cfg(test)]
mod worker_tests;
