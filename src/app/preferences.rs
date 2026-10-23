//! Settings drafts and transactional persistence with native shortcut/startup rollback.

use std::{cell::RefCell, fmt::Write as _, rc::Rc};

use anyhow::Result;
use sharer::{
    config::{AppConfig, SettingsMode, ShortcutConfig},
    storage::PlatformStorage,
};
use slint::ComponentHandle as _;

use crate::{
    AppWindow,
    hotkeys::{Hotkeys, shortcuts_enabled},
    views,
};

pub(super) fn wire_settings_callbacks(
    window: &AppWindow,
    hotkeys: &Rc<RefCell<Option<Hotkeys>>>,
    hotkey_status: &Rc<RefCell<String>>,
    config: &Rc<RefCell<AppConfig>>,
    storage: &Rc<PlatformStorage>,
) {
    wire_settings_mode_callback(window, config, storage);
    let save_window = window.as_weak();
    let save_hotkeys = Rc::clone(hotkeys);
    let save_status = Rc::clone(hotkey_status);
    let save_config = Rc::clone(config);
    let save_storage = Rc::clone(storage);

    window.on_save_settings(move || {
        if let Some(window) = save_window.upgrade() {
            if window.get_busy() {
                return;
            }

            finish_shortcut_recording(&window);

            match views::settings::config_from_window(&window) {
                Ok(next) => {
                    save_settings(
                        &window,
                        &save_hotkeys,
                        &save_status,
                        &save_config,
                        save_storage.as_ref(),
                        next,
                    );
                }

                Err(error) => {
                    window.set_status_text("Settings not saved".into());
                    window.set_status_detail(error.to_string().into());
                }
            }
        }
    });

    let discard_window = window.as_weak();
    let discard_config = Rc::clone(config);

    window.on_discard_settings(move || {
        if let Some(window) = discard_window.upgrade() {
            if window.get_busy() {
                return;
            }

            finish_shortcut_recording(&window);
            views::settings::initialize(&window, &discard_config.borrow());
            window.set_status_text("Changes discarded".into());
            window.set_status_detail("Settings match the saved configuration".into());
        }
    });
}

fn wire_settings_mode_callback(
    window: &AppWindow,
    config: &Rc<RefCell<AppConfig>>,
    storage: &Rc<PlatformStorage>,
) {
    let mode_window = window.as_weak();
    let mode_config = Rc::clone(config);
    let mode_storage = Rc::clone(storage);

    window.on_settings_mode_changed(move |advanced| {
        let Some(window) = mode_window.upgrade() else {
            return;
        };
        let mode = if advanced {
            SettingsMode::Advanced
        } else {
            SettingsMode::Simple
        };
        let previous = mode_config.borrow().settings_mode;

        if mode == previous {
            return;
        }

        let result = mode_config
            .borrow_mut()
            .save_settings_mode(mode_storage.as_ref(), mode);

        if let Err(error) = result {
            window.set_advanced_settings(previous == SettingsMode::Advanced);
            show_settings_error(
                &window,
                &format!("Could not remember settings view: {error}"),
            );
        }
    });
}

pub(super) fn set_recorded_shortcut(window: &AppWindow, shortcut: String) {
    let shortcut = shortcut.into();

    match window.get_shortcut_recording() {
        1 => window.set_region_hotkey(shortcut),
        2 => window.set_recording_hotkey(shortcut),
        3 => window.set_screen_hotkey(shortcut),
        4 => window.set_clipboard_hotkey(shortcut),
        _ => {}
    }
}

pub(super) fn cancel_shortcut_recording(
    window: &AppWindow,
    hotkeys: &Rc<RefCell<Option<Hotkeys>>>,
    hotkey_status: &RefCell<String>,
    config: &RefCell<AppConfig>,
    detail: &str,
) {
    window.set_shortcut_recording(0);
    restore_configured_hotkeys(window, hotkeys, hotkey_status, config);
    window.set_status_detail(detail.into());
}

pub(super) fn restore_configured_hotkeys(
    window: &AppWindow,
    hotkeys: &Rc<RefCell<Option<Hotkeys>>>,
    hotkey_status: &RefCell<String>,
    config: &RefCell<AppConfig>,
) {
    *hotkeys.borrow_mut() = None;
    let config = config.borrow();
    let status = match Hotkeys::register(&config.shortcuts) {
        Ok(registered) => {
            *hotkeys.borrow_mut() = Some(registered);
            shortcut_status(&config.shortcuts).to_owned()
        }

        Err(error) => format!("Shortcuts unavailable: {error}"),
    };

    status.clone_into(&mut hotkey_status.borrow_mut());
    window.set_hotkey_status(status.into());
}

pub(super) fn shortcut_status(shortcuts: &ShortcutConfig) -> &'static str {
    if shortcuts_enabled(shortcuts) {
        "Shortcuts active"
    } else {
        "Shortcuts disabled"
    }
}

pub(super) fn finish_shortcut_recording(window: &AppWindow) {
    if window.get_shortcut_recording() != 0 {
        window.set_shortcut_recording(0);
        window.invoke_shortcut_recording_finished();
    }
}

pub(super) fn save_settings(
    window: &AppWindow,
    hotkeys: &Rc<RefCell<Option<Hotkeys>>>,
    hotkey_status: &RefCell<String>,
    config: &RefCell<AppConfig>,
    storage: &PlatformStorage,
    next: AppConfig,
) -> bool {
    if window.get_busy() {
        return false;
    }

    let previous = config.borrow().clone();
    let startup_changed = next.behavior.start_at_login != previous.behavior.start_at_login;
    let shortcuts_changed = next.shortcuts != previous.shortcuts;

    if startup_changed
        && let Err(error) = crate::autostart::set_enabled(next.behavior.start_at_login)
    {
        show_settings_error(window, &error.to_string());

        return false;
    }

    if shortcuts_changed
        && let Err(error) = replace_hotkeys(hotkeys, &next.shortcuts, &previous.shortcuts)
    {
        let mut detail = error.to_string();

        append_startup_rollback_error(&mut detail, startup_changed, &previous);

        let status = if hotkeys.borrow().is_some() {
            if shortcuts_enabled(&previous.shortcuts) {
                format!("Previous shortcuts remain active: {error}")
            } else {
                format!("Shortcuts remain disabled: {error}")
            }
        } else {
            format!("Shortcuts unavailable: {error}")
        };

        status.clone_into(&mut hotkey_status.borrow_mut());
        window.set_hotkey_status(status.into());
        show_settings_error(window, &detail);

        return false;
    }

    if let Err(error) = next.save(storage) {
        let rollback_error = shortcuts_changed
            .then(|| replace_hotkeys(hotkeys, &previous.shortcuts, &next.shortcuts).err())
            .flatten();

        let mut detail = rollback_error.as_ref().map_or_else(
            || format!("Could not save settings: {error}"),
            |rollback_error| {
                format!(
                    "Could not save settings: {error}; shortcut rollback failed: {rollback_error}"
                )
            },
        );

        append_startup_rollback_error(&mut detail, startup_changed, &previous);

        if shortcuts_changed {
            let status = if let Some(rollback_error) = rollback_error {
                if hotkeys.borrow().is_some() {
                    format!("Unsaved shortcuts remain active: {rollback_error}")
                } else {
                    format!("Shortcuts unavailable after rollback: {rollback_error}")
                }
            } else {
                shortcut_status(&previous.shortcuts).to_owned()
            };

            status.clone_into(&mut hotkey_status.borrow_mut());
            window.set_hotkey_status(status.into());
        }

        show_settings_error(window, &detail);

        return false;
    }

    *config.borrow_mut() = next;
    views::settings::initialize(window, &config.borrow());

    if shortcuts_changed {
        let status = shortcut_status(&config.borrow().shortcuts);

        status.clone_into(&mut hotkey_status.borrow_mut());
        window.set_hotkey_status(status.into());
    }

    window.set_status_text("Ready".into());
    window.set_status_detail("Settings saved".into());

    true
}

fn append_startup_rollback_error(detail: &mut String, changed: bool, previous: &AppConfig) {
    if changed && let Err(error) = crate::autostart::set_enabled(previous.behavior.start_at_login) {
        let _ = write!(detail, "; Start at login could not be restored: {error}");
    }
}

pub(super) fn show_settings_error(window: &AppWindow, error: &str) {
    window.set_status_text("Settings not saved".into());
    window.set_status_detail(error.into());
}

fn replace_hotkeys(
    hotkeys: &Rc<RefCell<Option<Hotkeys>>>,
    shortcuts: &ShortcutConfig,
    fallback: &ShortcutConfig,
) -> Result<()> {
    *hotkeys.borrow_mut() = None;

    match Hotkeys::register(shortcuts) {
        Ok(registered) => {
            *hotkeys.borrow_mut() = Some(registered);
            Ok(())
        }

        Err(error) => {
            match Hotkeys::register(fallback) {
                Ok(registered) => *hotkeys.borrow_mut() = Some(registered),

                Err(fallback_error) => {
                    return Err(error.context(format!(
                        "previous shortcuts could not be restored: {fallback_error}"
                    )));
                }
            }

            Err(error)
        }
    }
}
