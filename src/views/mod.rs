//! Rust-side presenters for the separate Slint application views.

pub(super) mod confirmation;
pub(super) mod history;
pub(super) mod settings;

pub(super) fn initialize_presentation(window: &crate::AppWindow) {
    window.on_display_text(|value| {
        sharer::presentation::safe_text(value.as_str())
            .into_owned()
            .into()
    });
    window.set_presentation_ready(true);
}
