//! One-shot confirmations contained within the application window.

use std::cell::RefCell;

use slint::ComponentHandle as _;

use crate::AppWindow;
use sharer::presentation::safe_text;

pub(crate) struct Confirmation<'a> {
    pub title: &'a str,
    pub description: &'a str,
    pub target: &'a str,
    pub action_label: &'a str,
}

/// Keep the pending action tied to this window and consume it on either answer.
/// A second request cannot silently replace the action the user is reviewing.
pub(crate) fn request(
    window: &AppWindow,
    confirmation: &Confirmation<'_>,
    accepted: impl FnOnce(&AppWindow) + 'static,
) {
    if window.get_confirmation_visible() || window.get_ffmpeg_setup_visible() {
        return;
    }

    window.set_confirmation_title(confirmation.title.into());
    window.set_confirmation_description(confirmation.description.into());
    window.set_confirmation_target(safe_text(confirmation.target).as_ref().into());
    window.set_confirmation_action_label(confirmation.action_label.into());

    let pending = RefCell::new(Some(accepted));
    let weak_window = window.as_weak();

    window.on_confirmation_response(move |confirmed| {
        let Some(action) = pending.borrow_mut().take() else {
            return;
        };
        let Some(window) = weak_window.upgrade() else {
            return;
        };
        let was_visible = window.get_confirmation_visible();

        window.set_confirmation_visible(false);
        window.set_confirmation_title("".into());
        window.set_confirmation_description("".into());
        window.set_confirmation_target("".into());
        window.set_confirmation_action_label("".into());

        if confirmed && was_visible {
            action(&window);
        }
    });
    window.set_confirmation_visible(true);
}

pub(crate) fn deletion_page(
    window: &AppWindow,
    target: &str,
    accepted: impl FnOnce(&AppWindow) + 'static,
) {
    request(
        window,
        &Confirmation {
            title: "Open the deletion page?",
            description: "This opens the host's deletion page in your browser. The uploaded file may be deleted immediately and cannot be restored. The local copy and history entry stay.",
            target,
            action_label: "Open deletion page",
        },
        accepted,
    );
}

#[cfg(test)]
mod tests;
