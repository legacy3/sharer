//! Pending actions never outlive cancellation or silently change their target.

use std::{cell::Cell, rc::Rc};

use super::*;

struct ConfirmationTestPlatform;

impl slint::platform::Platform for ConfirmationTestPlatform {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::platform::PlatformError> {
        use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};

        Ok(MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer))
    }
}

fn window() -> AppWindow {
    slint::platform::set_platform(Box::new(ConfirmationTestPlatform)).ok();
    AppWindow::new().unwrap()
}

#[test]
fn cancel_drops_pending_action_and_later_accept_does_nothing() {
    let window = window();
    let calls = Rc::new(Cell::new(0));
    let observed = Rc::clone(&calls);

    deletion_page(&window, "capture.png", move |_| {
        observed.set(observed.get() + 1);
    });
    assert!(window.get_confirmation_visible());
    window.invoke_confirmation_response(false);
    window.invoke_confirmation_response(true);
    assert!(!window.get_confirmation_visible());
    assert_eq!(calls.get(), 0);
}

#[test]
fn accept_runs_once_after_modal_is_closed() {
    let window = window();
    let calls = Rc::new(Cell::new(0));
    let observed = Rc::clone(&calls);

    deletion_page(&window, "capture.png", move |window| {
        assert!(!window.get_confirmation_visible());
        observed.set(observed.get() + 1);
    });
    window.invoke_confirmation_response(true);
    window.invoke_confirmation_response(true);
    assert_eq!(calls.get(), 1);
}

#[test]
fn another_request_cannot_replace_visible_target_or_pending_action() {
    let window = window();
    let target = Rc::new(Cell::new(0));
    let first = Rc::clone(&target);
    let second = Rc::clone(&target);

    deletion_page(&window, "first.png", move |_| first.set(1));
    deletion_page(&window, "second.png", move |_| second.set(2));
    assert_eq!(window.get_confirmation_target(), "first.png");
    window.invoke_confirmation_response(true);
    assert_eq!(target.get(), 1);
}

#[test]
fn untrusted_target_is_sanitized_and_bounded_without_capability_url() {
    let window = window();
    let target = format!("capture\n\u{202e}{}", "é".repeat(5_000));

    deletion_page(&window, &target, |_| {});
    assert!(
        window
            .get_confirmation_target()
            .starts_with("capture\\n\\u{202e}")
    );
    assert!(window.get_confirmation_target().ends_with('…'));
    assert!(!window.get_confirmation_target().contains('\n'));
    assert!(!window.get_confirmation_description().contains("https://"));
}

#[test]
fn wizard_state_survives_a_cancelled_quit_confirmation() {
    let window = window();

    window.set_show_onboarding(true);
    window.set_onboarding_step(2);
    window.set_wizard_uploader_url("https://draft.example/upload".into());
    deletion_page(&window, "capture.png", |_| {});
    assert!(window.get_confirmation_visible());
    window.invoke_confirmation_response(false);
    assert!(window.get_show_onboarding());
    assert_eq!(window.get_onboarding_step(), 2);
    assert_eq!(
        window.get_wizard_uploader_url(),
        "https://draft.example/upload"
    );
    assert!(window.get_confirmation_target().is_empty());
}
