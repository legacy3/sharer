//! Paginated upload-history presentation and row actions.

use std::{cell::RefCell, rc::Rc};

use anyhow::{Context as _, Result};
use slint::{ComponentHandle as _, Model as _, ModelRc, SharedString, VecModel};

use crate::{AppWindow, HistoryRow};
use sharer::{history::HistoryEntry, storage::PlatformStorage};

const PAGE_SIZE: usize = 25;

pub(crate) struct HistoryController {
    page: usize,
    total: u64,
    rows: Rc<VecModel<HistoryRow>>,
    suspended: bool,
}

pub(crate) fn initialize(
    window: &AppWindow,
    storage: &Rc<PlatformStorage>,
    suspended: bool,
) -> Result<Rc<RefCell<HistoryController>>> {
    let rows = Rc::new(VecModel::default());
    let controller = Rc::new(RefCell::new(HistoryController {
        page: 0,
        total: 0,
        rows: Rc::clone(&rows),
        suspended,
    }));

    window.set_history_rows(ModelRc::from(rows));

    if !suspended {
        refresh(window, storage, &controller, 0)?;
    }

    wire_callbacks(window, storage, &controller);

    Ok(controller)
}

pub(crate) fn observe_inserted(
    window: &AppWindow,
    controller: &RefCell<HistoryController>,
    entry: &HistoryEntry,
) {
    let mut controller = controller.borrow_mut();

    if cache_insert(&mut controller, entry) {
        update_pagination(window, &controller);
    }
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

fn wire_callbacks(
    window: &AppWindow,
    storage: &Rc<PlatformStorage>,
    controller: &Rc<RefCell<HistoryController>>,
) {
    let window_weak = window.as_weak();
    let controller_for_open = Rc::clone(controller);

    window.on_open_history_link(move |index| {
        let Some(row) = row_at(&controller_for_open, index) else {
            return;
        };

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
    let total = storage.history_count()?;
    let page_count = page_count(total);
    let page = requested_page.min(page_count.saturating_sub(1));
    let offset = page
        .checked_mul(PAGE_SIZE)
        .context("history page offset overflowed")?;
    let rows = storage
        .history_page(offset, PAGE_SIZE)?
        .iter()
        .map(row_from_entry)
        .collect::<Vec<_>>();
    let mut controller = controller.borrow_mut();

    controller.page = page;
    controller.total = total;
    controller.suspended = false;
    controller.rows.set_vec(rows);
    update_pagination(window, &controller);

    Ok(())
}

fn cache_insert(controller: &mut HistoryController, entry: &HistoryEntry) -> bool {
    if controller.suspended {
        return false;
    }

    controller.total = controller.total.saturating_add(1);

    if controller.page == 0 {
        controller.rows.insert(0, row_from_entry(entry));

        if controller.rows.row_count() > PAGE_SIZE {
            controller.rows.remove(PAGE_SIZE);
        }
    }

    true
}

fn update_pagination(window: &AppWindow, controller: &HistoryController) {
    let pages = page_count(controller.total);

    window.set_history_page_label(
        format!(
            "Page {} of {pages}  -  {} uploads",
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
        filename: entry.filename.as_str().into(),
        size: format_size(entry.size_bytes).into(),
        expires: entry.expires_at.as_str().into(),
        link: entry.link.as_str().into(),
        delete_link: entry.delete_url.as_str().into(),
    }
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
    use std::rc::Rc;

    use sharer::history::HistoryEntry;
    use slint::{Model as _, VecModel};

    use super::{HistoryController, cache_insert, format_size, page_count};
    use crate::HistoryRow;

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
    fn suspended_history_does_not_cache_background_uploads() {
        let rows = Rc::new(VecModel::<HistoryRow>::default());
        let mut controller = HistoryController {
            page: 0,
            total: 3,
            rows: Rc::clone(&rows),
            suspended: true,
        };
        let entry = HistoryEntry {
            link: "https://uploads.example/file".to_owned(),
            delete_url: "https://uploads.example/delete".to_owned(),
            filename: "capture.png".to_owned(),
            size_bytes: 42,
            expires_at: "Never".to_owned(),
        };

        assert!(!cache_insert(&mut controller, &entry));
        assert_eq!(rows.row_count(), 0);
        assert_eq!(controller.total, 3);
    }
}
