use super::*;

fn previous_artifact_window() -> (AppWindow, Rc<Cell<usize>>) {
    slint::platform::set_platform(Box::new(crate::app::tests::SoftwareTestPlatform)).ok();
    let window = AppWindow::new().unwrap();
    let cleared = Rc::new(Cell::new(0));
    let cleared_callback = Rc::clone(&cleared);

    window.on_receipt_cleared(move || cleared_callback.set(cleared_callback.get() + 1));
    window.set_auto_upload_captures(false);
    window.set_save_captures(true);
    window.set_last_local_path("previous.png".into());
    window.set_last_link("https://public.example/previous".into());
    window.set_last_delete_url("https://delete.example/private-unsaved-capability".into());
    window.set_last_delete_saved(false);
    window.set_artifact_status_text("Uploaded; history not saved".into());
    window.set_artifact_status_detail("Keep the private deletion link".into());
    window.set_artifact_action_detail("Could not copy public link".into());
    window.set_artifact_action_error(true);
    window.set_history_action_detail("Earlier history action failed".into());
    window.set_history_action_failed(true);
    window.set_artifact_preview_state(2);
    window.set_artifact_name("previous.png".into());
    window.set_artifact_description("PNG preview".into());
    let mut preview = slint::SharedPixelBuffer::new(1, 1);

    preview
        .make_mut_bytes()
        .copy_from_slice(&[82, 224, 191, 255]);
    window.set_artifact_preview(slint::Image::from_rgba8(preview));
    (window, cleared)
}

fn assert_previous_artifact(window: &AppWindow) {
    assert_eq!(window.get_last_local_path(), "previous.png");
    assert_eq!(window.get_last_link(), "https://public.example/previous");
    assert_eq!(
        window.get_last_delete_url(),
        "https://delete.example/private-unsaved-capability"
    );
    assert!(!window.get_last_delete_saved());
    assert_eq!(
        window.get_artifact_status_text(),
        "Uploaded; history not saved"
    );
    assert_eq!(
        window.get_artifact_status_detail(),
        "Keep the private deletion link"
    );
    assert_eq!(window.get_artifact_preview_state(), 2);
    let preview = window.get_artifact_preview().to_rgba8().unwrap();

    assert_eq!((preview.width(), preview.height()), (1, 1));
    assert_eq!(preview.as_bytes(), &[82, 224, 191, 255]);
    assert_eq!(window.get_artifact_name(), "previous.png");
    assert_eq!(window.get_artifact_description(), "PNG preview");
}

fn queue_prepared_capture(window: &AppWindow, sender: &Sender<Job>) {
    super::super::queue_capture_job(
        window,
        sender,
        JobSource::Prepared {
            payload: sharer::upload::UploadPayload::from_bytes(
                vec![1],
                "next.png".to_owned(),
                "image/png".to_owned(),
            ),
            save_directory: Some(PathBuf::from("captures")),
        },
        Arc::new(UploadCancellation::default()),
    );
}

#[test]
fn escape_and_confirmed_capture_preserve_the_previous_artifact() {
    use slint::platform::{Key, WindowEvent};

    let (window, cleared) = previous_artifact_window();
    let selector = RegionWindow::new().unwrap();
    let holders = Rc::new(RefCell::new(vec![selector.clone_strong()]));

    begin_selection_activity(&window, "Drag to capture, or press Escape");
    assert!(window.get_busy());
    assert_eq!(cleared.get(), 0);
    assert_eq!(window.get_artifact_preview_state(), 2);
    wire_region_cancel(&selector, &window, &holders, false);
    selector.show().unwrap();
    selector.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Escape.into(),
    });
    selector.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Escape.into(),
    });

    assert!(holders.borrow().is_empty());
    assert!(!window.get_busy());
    assert_eq!(window.get_status_detail(), "Region selection cancelled");
    assert_eq!(cleared.get(), 0);
    assert_previous_artifact(&window);
    assert!(!window.get_artifact_is_previous());
    assert_eq!(
        window.get_artifact_action_detail(),
        "Could not copy public link"
    );
    assert!(window.get_artifact_action_error());
    assert_eq!(
        window.get_history_action_detail(),
        "Earlier history action failed"
    );
    assert!(window.get_history_action_failed());
    let (sender, receiver) = mpsc::channel();

    // The native macOS selector returns a prepared payload through this same
    // queue path. Confirmation clears stale action feedback, while the previous
    // receipt remains recoverable until the new attempt produces a replacement.
    queue_prepared_capture(&window, &sender);
    let job = receiver.try_recv().unwrap();

    assert!(matches!(job.source, JobSource::Prepared { .. }));
    assert!(window.get_busy());
    assert_eq!(cleared.get(), 0);
    assert_previous_artifact(&window);
    assert!(window.get_artifact_is_previous());
    assert!(window.get_artifact_action_detail().is_empty());
    assert!(!window.get_artifact_action_error());
    assert!(window.get_history_action_detail().is_empty());
    assert!(!window.get_history_action_failed());
}

#[test]
fn stopped_capture_worker_preserves_the_previous_artifact() {
    let (window, cleared) = previous_artifact_window();
    let (sender, receiver) = mpsc::channel();

    drop(receiver);
    queue_prepared_capture(&window, &sender);
    assert!(!window.get_busy());
    assert_eq!(window.get_status_text(), "Capture failed");
    assert_eq!(
        window.get_status_detail(),
        "Capture worker stopped unexpectedly"
    );
    assert_eq!(cleared.get(), 0);
    assert_previous_artifact(&window);
    assert!(window.get_artifact_is_previous());
    assert!(window.get_artifact_action_detail().is_empty());
    assert!(!window.get_artifact_action_error());
    assert!(window.get_history_action_detail().is_empty());
    assert!(!window.get_history_action_failed());
}
