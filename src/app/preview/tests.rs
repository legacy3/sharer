use std::{
    io::Cursor,
    path::PathBuf,
    sync::{
        Arc,
        atomic::AtomicU64,
        mpsc::{self, Receiver, SyncSender},
    },
    time::Duration,
};

use image::{DynamicImage, ImageEncoder as _, ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};

use super::{
    MAX_SOURCE_BYTES, PREVIEW_EMPTY, PREVIEW_LOADING, PREVIEW_READY, PREVIEW_UNAVAILABLE,
    PREVIEW_UNSUPPORTED, PreviewArtifact, PreviewReply, PreviewRequest, Previewer,
    THUMBNAIL_HEIGHT, THUMBNAIL_WIDTH, clear, decode_artifact, load_artifact, unavailable,
};
use crate::{AppWindow, app::tests::SoftwareTestPlatform};

fn window() -> AppWindow {
    slint::platform::set_platform(Box::new(SoftwareTestPlatform)).ok();
    AppWindow::new().unwrap()
}

fn controlled_previewer() -> (
    Previewer,
    Receiver<PreviewRequest>,
    SyncSender<PreviewReply>,
) {
    let (requests, request_rx) = mpsc::sync_channel(1);
    let (reply_tx, replies) = mpsc::sync_channel(1);
    let previewer = Previewer {
        requests,
        replies,
        current_generation: Arc::new(AtomicU64::new(0)),
        generation: 0,
        path: PathBuf::new(),
        pending: None,
    };

    (previewer, request_rx, reply_tx)
}

fn sample_artifact(description: &str) -> PreviewArtifact {
    PreviewArtifact {
        state: PREVIEW_READY,
        description: description.to_owned(),
        image: Some(RgbaImage::from_pixel(4, 3, Rgba([40, 180, 200, 255]))),
    }
}

#[test]
fn png_thumbnail_is_bounded_and_preserves_aspect_ratio() {
    let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(1920, 1080, Rgba([2, 4, 6, 255])));
    let mut bytes = Cursor::new(Vec::new());

    image.write_to(&mut bytes, ImageFormat::Png).unwrap();
    let artifact = decode_artifact(bytes.get_ref()).unwrap();
    let thumbnail = artifact.image.unwrap();

    assert_eq!(artifact.state, PREVIEW_READY);
    assert_eq!(thumbnail.dimensions(), (THUMBNAIL_WIDTH, THUMBNAIL_HEIGHT));
    assert!(thumbnail.as_raw().len() <= 640 * 360 * 4);
    assert!(artifact.description.contains("1920 x 1080"));
    assert_eq!(thumbnail.get_pixel(0, 0), &Rgba([2, 4, 6, 255]));
}

#[test]
fn jpeg_orientation_is_applied_to_the_bounded_thumbnail_and_original_dimensions() {
    let image = RgbImage::from_pixel(800, 400, Rgb([50, 100, 150]));

    for orientation in 1_u8..=8 {
        let mut bytes = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new(&mut bytes);
        // Little-endian TIFF with one Orientation SHORT value; 5..8 swap axes.
        let exif = vec![
            0x49,
            0x49,
            0x2a,
            0,
            8,
            0,
            0,
            0,
            1,
            0,
            0x12,
            1,
            3,
            0,
            1,
            0,
            0,
            0,
            orientation,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ];

        encoder.set_exif_metadata(exif).unwrap();
        encoder.encode_image(&image).unwrap();
        let artifact = decode_artifact(&bytes).unwrap();
        let thumbnail = artifact.image.unwrap();
        let expected = if orientation >= 5 {
            (400, 800)
        } else {
            (800, 400)
        };
        let expected_thumbnail = if orientation >= 5 {
            (180, 360)
        } else {
            (640, 320)
        };

        assert_eq!(artifact.state, PREVIEW_READY);
        assert_eq!(
            thumbnail.dimensions(),
            expected_thumbnail,
            "EXIF orientation {orientation}"
        );
        assert!(
            artifact
                .description
                .contains(&format!("JPEG · {} x {} ·", expected.0, expected.1)),
            "EXIF orientation {orientation}: {}",
            artifact.description
        );
        assert!(thumbnail.width() <= THUMBNAIL_WIDTH);
        assert!(thumbnail.height() <= THUMBNAIL_HEIGHT);
    }
}

#[test]
fn huge_image_headers_are_rejected_before_pixel_decoding() {
    let width_error = decode_artifact(include_bytes!("fixtures/oversized-width.png"))
        .unwrap_err()
        .to_string();
    let pixel_error = decode_artifact(include_bytes!("fixtures/oversized-pixels.png"))
        .unwrap_err()
        .to_string();
    let decoded_error = decode_artifact(include_bytes!("fixtures/oversized-decoded.png"))
        .unwrap_err()
        .to_string();

    assert!(width_error.contains("limit"));
    assert!(pixel_error.contains("preview pixel limit"));
    assert!(decoded_error.contains("preview decoded-byte limit"));
}

#[test]
fn unsupported_and_malformed_files_never_produce_fake_previews() {
    let artifact = decode_artifact(b"a plain text artifact").unwrap();

    assert_eq!(artifact.state, PREVIEW_UNSUPPORTED);
    assert!(artifact.image.is_none());
    assert!(decode_artifact(b"\x89PNG\r\n\x1a\ntruncated").is_err());
    assert!(decode_artifact(b"\xff\xd8\xff\xe0truncated").is_err());
}

#[test]
fn source_size_missing_files_and_directories_are_handled_without_decoding() {
    let directory = tempfile::tempdir().unwrap();
    let file = tempfile::NamedTempFile::new_in(directory.path()).unwrap();

    file.as_file().set_len(MAX_SOURCE_BYTES + 1).unwrap();
    let artifact = load_artifact(file.path()).unwrap();

    assert_eq!(artifact.state, PREVIEW_UNAVAILABLE);
    assert!(artifact.image.is_none());
    assert!(artifact.description.contains("32 MiB"));
    assert!(load_artifact(&directory.path().join("missing.png")).is_err());
    assert!(load_artifact(directory.path()).is_err());
}

#[cfg(unix)]
#[test]
fn preview_does_not_follow_symbolic_links() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap();
    let target = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
    let link = directory.path().join("capture.png");

    symlink(target.path(), &link).unwrap();
    assert!(load_artifact(&link).is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn preview_rejects_fifos_without_waiting_for_a_writer() {
    let directory = tempfile::tempdir().unwrap();
    let fifo = directory.path().join("capture-pipe.png");

    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        &fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .unwrap();
    assert!(load_artifact(&fifo).is_err());
}

#[test]
fn stale_reply_never_replaces_a_new_receipt_or_changes_job_status() {
    let window = window();
    let (mut previewer, requests, replies) = controlled_previewer();

    window.set_last_local_path("first.png".into());
    previewer.refresh(&window);
    let old_request = requests.try_recv().unwrap();

    window.set_last_local_path("second.png".into());
    window.set_status_text("Uploaded".into());
    window.set_status_detail("Link copied to clipboard".into());
    previewer.refresh(&window);
    let current_request = requests.try_recv().unwrap();

    replies
        .send(PreviewReply {
            request: old_request,
            artifact: sample_artifact("old result"),
        })
        .unwrap();
    previewer.refresh(&window);
    assert_eq!(window.get_artifact_preview_state(), PREVIEW_LOADING);
    assert!(window.get_artifact_description().is_empty());
    replies
        .send(PreviewReply {
            request: current_request,
            artifact: unavailable("No preview for this file"),
        })
        .unwrap();
    previewer.refresh(&window);
    assert_eq!(window.get_artifact_preview_state(), PREVIEW_UNAVAILABLE);
    assert_eq!(window.get_status_text(), "Uploaded");
    assert_eq!(window.get_status_detail(), "Link copied to clipboard");
}

#[test]
fn clearing_and_reusing_the_same_path_invalidates_old_bytes() {
    let window = window();
    let (mut previewer, requests, replies) = controlled_previewer();

    window.set_last_local_path("capture.png".into());
    previewer.refresh(&window);
    let old_request = requests.try_recv().unwrap();

    clear(&window);
    previewer.refresh(&window);
    let current_request = requests.try_recv().unwrap();

    assert_ne!(old_request.generation, current_request.generation);
    replies
        .send(PreviewReply {
            request: old_request,
            artifact: sample_artifact("old bytes"),
        })
        .unwrap();
    previewer.refresh(&window);
    assert_eq!(window.get_artifact_preview_state(), PREVIEW_LOADING);
    replies
        .send(PreviewReply {
            request: current_request,
            artifact: sample_artifact("new bytes"),
        })
        .unwrap();
    previewer.refresh(&window);
    assert_eq!(window.get_artifact_description(), "new bytes");
    assert_eq!(window.get_artifact_preview_state(), PREVIEW_READY);
    window.set_last_local_path("".into());
    clear(&window);
    previewer.refresh(&window);
    assert_eq!(window.get_artifact_preview_state(), PREVIEW_EMPTY);
    assert_eq!(window.get_artifact_preview().size().width, 0);
}

#[test]
fn recreated_window_requests_the_same_path_again_without_an_empty_tick() {
    let first_window = window();
    let (mut previewer, requests, replies) = controlled_previewer();

    first_window.set_last_local_path("capture.png".into());
    previewer.refresh(&first_window);
    let first_request = requests.try_recv().unwrap();
    let second_window = window();

    second_window.set_last_local_path("capture.png".into());
    previewer.refresh(&second_window);
    let second_request = requests.try_recv().unwrap();

    assert_ne!(first_request.generation, second_request.generation);
    replies
        .send(PreviewReply {
            request: first_request,
            artifact: sample_artifact("from old window"),
        })
        .unwrap();
    previewer.refresh(&second_window);
    assert_eq!(second_window.get_artifact_preview_state(), PREVIEW_LOADING);
}

#[test]
fn saturated_request_queue_retains_only_the_newest_pending_path() {
    let window = window();
    let (mut previewer, requests, _replies) = controlled_previewer();

    window.set_last_local_path("first.png".into());
    previewer.refresh(&window);
    window.set_last_local_path("second.png".into());
    previewer.refresh(&window);
    window.set_last_local_path("newest.png".into());
    previewer.refresh(&window);
    let first = requests.try_recv().unwrap();

    assert_eq!(first.path, PathBuf::from("first.png"));
    previewer.refresh(&window);
    let newest = requests.try_recv().unwrap();

    assert_eq!(newest.path, PathBuf::from("newest.png"));
    assert!(requests.try_recv().is_err());
    assert!(previewer.pending.is_none());
}

#[test]
fn suspension_drains_completed_images_and_releases_the_queue() {
    let window = window();
    let (mut previewer, requests, replies) = controlled_previewer();

    window.set_last_local_path("capture.png".into());
    previewer.refresh(&window);
    let request = requests.try_recv().unwrap();

    replies
        .send(PreviewReply {
            request,
            artifact: sample_artifact("completed while hidden"),
        })
        .unwrap();
    previewer.suspend();
    assert!(previewer.replies.try_recv().is_err());
    assert!(previewer.path.as_os_str().is_empty());
    assert!(previewer.pending.is_none());
}

#[test]
fn real_worker_decodes_a_saved_image() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("local.png");
    let image = RgbaImage::from_pixel(8, 6, Rgba([20, 30, 40, 255]));

    image.save(&path).unwrap();
    let window = window();
    let mut previewer = Previewer::new();

    window.set_last_local_path(path.to_string_lossy().as_ref().into());
    previewer.refresh(&window);
    let reply = previewer
        .replies
        .recv_timeout(Duration::from_secs(5))
        .unwrap();

    assert!(previewer.accepts(&reply));
    assert_eq!(reply.artifact.state, PREVIEW_READY);
    assert_eq!(reply.artifact.image.unwrap().dimensions(), (8, 6));
}

#[test]
fn disconnected_worker_finishes_loading_with_an_unavailable_preview() {
    let window = window();
    let (mut previewer, requests, replies) = controlled_previewer();

    window.set_last_local_path("capture.png".into());
    previewer.refresh(&window);
    let _request = requests.try_recv().unwrap();

    drop(replies);
    previewer.refresh(&window);
    assert_eq!(window.get_artifact_preview_state(), PREVIEW_UNAVAILABLE);
    assert_eq!(window.get_status_text(), "Ready");
}
