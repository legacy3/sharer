//! Setup completion, replay, and reset without changing saved preferences until confirmation.

use std::{cell::RefCell, rc::Rc};

use sharer::{config::AppConfig, storage::PlatformStorage};
use slint::ComponentHandle as _;

use crate::{AppWindow, hotkeys::Hotkeys, views};

use super::preferences::{finish_shortcut_recording, save_settings, show_settings_error};

pub(super) fn wire_onboarding_callbacks(
    window: &AppWindow,
    hotkeys: &Rc<RefCell<Option<Hotkeys>>>,
    hotkey_status: &Rc<RefCell<String>>,
    config: &Rc<RefCell<AppConfig>>,
    storage: &Rc<PlatformStorage>,
) {
    wire_onboarding_finish_callbacks(window, hotkeys, hotkey_status, config, storage);
    wire_onboarding_reset_callbacks(window, config);
}

fn wire_onboarding_finish_callbacks(
    window: &AppWindow,
    hotkeys: &Rc<RefCell<Option<Hotkeys>>>,
    hotkey_status: &Rc<RefCell<String>>,
    config: &Rc<RefCell<AppConfig>>,
    storage: &Rc<PlatformStorage>,
) {
    let finish_window = window.as_weak();
    let finish_hotkeys = Rc::clone(hotkeys);
    let finish_status = Rc::clone(hotkey_status);
    let finish_config = Rc::clone(config);
    let finish_storage = Rc::clone(storage);

    let continue_window = window.as_weak();
    let continue_config = Rc::clone(config);

    window.on_onboarding_continue(move || {
        if let Some(window) = continue_window.upgrade() {
            match views::settings::wizard_config_from_window(&window, &continue_config.borrow()) {
                Ok(_) => {
                    window.set_onboarding_error("".into());
                    window.set_onboarding_step(1);
                }

                Err(error) => window.set_onboarding_error(error.to_string().into()),
            }
        }
    });

    window.on_onboarding_finish(move || {
        let Some(window) = finish_window.upgrade() else {
            return;
        };

        if window.get_busy() {
            return;
        }

        let next =
            match views::settings::wizard_config_from_window(&window, &finish_config.borrow()) {
                Ok(next) => next,

                Err(error) => {
                    window.set_onboarding_step(0);
                    window.set_onboarding_error(error.to_string().into());

                    return;
                }
            };

        if save_settings(
            &window,
            &finish_hotkeys,
            &finish_status,
            &finish_config,
            finish_storage.as_ref(),
            next,
        ) {
            window.set_status_detail("Setup saved. You can change this later in Settings.".into());
        } else {
            window.set_onboarding_error(window.get_status_detail());
        }
    });

    let skip_window = window.as_weak();
    let skip_hotkeys = Rc::clone(hotkeys);
    let skip_status = Rc::clone(hotkey_status);
    let skip_config = Rc::clone(config);
    let skip_storage = Rc::clone(storage);

    window.on_onboarding_skip(move || {
        let Some(window) = skip_window.upgrade() else {
            return;
        };

        if window.get_busy() {
            return;
        }

        if skip_config.borrow().behavior.onboarding_complete {
            window.set_show_onboarding(false);
            window.set_onboarding_error("".into());

            return;
        }

        let mut next = skip_config.borrow().clone();

        next.behavior.onboarding_complete = true;
        next.behavior.auto_upload_captures = false;
        next.behavior.save_captures = true;

        if let Err(error) = views::settings::ensure_default_capture_directory(&mut next) {
            window.set_onboarding_error(error.to_string().into());

            return;
        }

        if save_settings(
            &window,
            &skip_hotkeys,
            &skip_status,
            &skip_config,
            skip_storage.as_ref(),
            next,
        ) {
            window.set_show_onboarding(false);
            window.set_onboarding_error("".into());
            window.set_status_text("Ready".into());
            window.set_status_detail(
                "Captures stay on this computer. Add a provider later in Settings.".into(),
            );
        } else {
            window.set_onboarding_error(window.get_status_detail());
        }
    });
}

fn wire_onboarding_reset_callbacks(window: &AppWindow, config: &Rc<RefCell<AppConfig>>) {
    let restart_window = window.as_weak();
    let restart_config = Rc::clone(config);

    window.on_onboarding_restart(move || {
        let Some(window) = restart_window.upgrade() else {
            return;
        };

        if !can_open_setup(&window) {
            return;
        }

        views::settings::initialize_wizard(&window, &restart_config.borrow(), false);
        window.set_show_onboarding(true);
    });

    let reset_window = window.as_weak();
    let reset_config = Rc::clone(config);

    window.on_reset_settings(move || {
        let Some(window) = reset_window.upgrade() else {
            return;
        };

        if !can_open_setup(&window) {
            return;
        }

        let defaults = views::settings::reset_preferences(&reset_config.borrow());

        views::settings::initialize_wizard(&window, &defaults, true);
        window.set_show_onboarding(true);
    });
}

fn can_open_setup(window: &AppWindow) -> bool {
    if window.get_busy() || window.get_confirmation_visible() {
        return false;
    }

    if window.get_settings_dirty() {
        show_settings_error(
            window,
            "Save or discard your settings changes before opening setup.",
        );

        return false;
    }

    finish_shortcut_recording(window);
    true
}
