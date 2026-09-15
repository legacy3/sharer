use std::{
    net::TcpListener,
    sync::{Arc, mpsc},
    thread,
};

use super::*;

fn stalled_server(
    send_partial_response: bool,
) -> (String, mpsc::Receiver<()>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (accepted_tx, accepted_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request);

        accepted_tx.send(()).unwrap();

        if send_partial_response {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\nConnection: close\r\n\r\nx")
                .unwrap();
            stream.flush().unwrap();
        }

        thread::sleep(Duration::from_millis(500));
    });

    (format!("http://{address}/upload"), accepted_rx, server)
}

fn non_reading_server() -> (String, mpsc::Receiver<()>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (accepted_tx, accepted_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (_stream, _) = listener.accept().unwrap();

        accepted_tx.send(()).unwrap();
        thread::sleep(Duration::from_millis(500));
    });

    (format!("http://{address}/upload"), accepted_rx, server)
}

fn short_timeout_client(endpoint: &str, read: Duration, total: Duration) -> UploadClient {
    let target = UploadTarget {
        kind: UploaderKind::Custom,
        custom_url: endpoint.to_owned(),
        ..UploadTarget::default()
    };

    UploadClient::for_target_with_timeouts(
        &target,
        &UploadRoute::Direct,
        UploadTimeouts {
            connect: Duration::from_secs(1),
            read,
            total,
        },
    )
    .unwrap()
}

fn small_payload() -> UploadPayload {
    UploadPayload::from_bytes(
        vec![1, 2, 3],
        "sample.bin".to_owned(),
        "application/octet-stream".to_owned(),
    )
}

#[test]
fn lifetime_is_appended_as_seconds() {
    let endpoint = Url::parse("https://uploads.example/upload?source=desktop").unwrap();
    let url = endpoint_with_lifetime(&endpoint, 3_600);

    assert_eq!(
        url.as_str(),
        "https://uploads.example/upload?source=desktop&time=3600"
    );
}

#[test]
fn configured_lifetime_is_replaced() {
    let endpoint = Url::parse("https://uploads.example/upload?time=12&token=public").unwrap();
    let url = endpoint_with_lifetime(&endpoint, 60);

    assert_eq!(
        url.as_str(),
        "https://uploads.example/upload?token=public&time=60"
    );
}

#[test]
fn stalled_upload_can_be_cancelled() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("large-sparse.bin");

    std::fs::File::create(&path)
        .unwrap()
        .set_len(512 * 1024 * 1024)
        .unwrap();
    let payload = UploadPayload::from_path(&path).unwrap();
    let (endpoint, accepted, server) = non_reading_server();
    let client = short_timeout_client(&endpoint, Duration::from_secs(2), Duration::from_secs(2));
    let cancellation = Arc::new(UploadCancellation::default());
    let cancellation_for_upload = Arc::clone(&cancellation);
    let upload = thread::spawn(move || {
        client.upload_with_progress_and_cancellation(
            payload,
            60,
            cancellation_for_upload.as_ref(),
            |_, _| {},
        )
    });

    accepted.recv_timeout(Duration::from_secs(1)).unwrap();
    let cancellation_started = std::time::Instant::now();

    cancellation.cancel();
    let error = upload.join().unwrap().unwrap_err();

    assert!(is_upload_cancelled(error.as_ref()));
    assert!(cancellation_started.elapsed() < Duration::from_millis(400));
    server.join().unwrap();
}

#[test]
fn total_timeout_bounds_a_server_that_never_responds() {
    let (endpoint, accepted, server) = stalled_server(false);
    let client = short_timeout_client(
        &endpoint,
        Duration::from_secs(1),
        Duration::from_millis(100),
    );
    let error = client.upload(small_payload(), 60).unwrap_err();

    accepted.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(is_upload_timeout(error.as_ref()));
    server.join().unwrap();
}

#[test]
fn read_timeout_bounds_a_stalled_response_body() {
    let (endpoint, accepted, server) = stalled_server(true);
    let client = short_timeout_client(
        &endpoint,
        Duration::from_millis(100),
        Duration::from_secs(2),
    );
    let error = client.upload(small_payload(), 60).unwrap_err();

    accepted.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(is_upload_timeout(error.as_ref()));
    server.join().unwrap();
}

#[test]
fn response_links_must_be_absolute_http_urls() {
    assert!(validate_response_url("https://files.example/a", "public link").is_ok());
    assert!(validate_response_url("file:///tmp/secret", "public link").is_err());
    assert!(validate_response_url("javascript:alert(1)", "public link").is_err());
    assert!(validate_response_url("/relative", "public link").is_err());
}

#[test]
fn error_detail_truncation_preserves_utf8_boundaries() {
    let body = "\u{00e9}".repeat(301);
    let detail = body
        .char_indices()
        .nth(300)
        .map_or(body.as_str(), |(boundary, _character)| &body[..boundary]);

    assert_eq!(detail.chars().count(), 300);
}

#[test]
fn generated_payload_retains_bytes() {
    let payload = UploadPayload::from_bytes(
        vec![1, 2, 3],
        "sample.bin".to_owned(),
        "application/octet-stream".to_owned(),
    );

    assert_eq!(payload.bytes(), Some([1, 2, 3].as_slice()));
}

#[test]
fn response_body_is_bounded() {
    let accepted = vec![0_u8; MAX_RESPONSE_BYTES];
    let rejected = vec![0_u8; MAX_RESPONSE_BYTES + 1];

    assert_eq!(
        read_response_body(Cursor::new(accepted)).unwrap().len(),
        MAX_RESPONSE_BYTES
    );
    assert!(read_response_body(Cursor::new(rejected)).is_err());
}

#[test]
fn progress_callback_count_is_bounded_by_percentage() {
    use std::sync::{Arc, Mutex};

    let events = Arc::new(Mutex::new(Vec::new()));
    let captured_events = Arc::clone(&events);
    let source = Cursor::new(vec![0_u8; 1024 * 1024]);
    let mut reader = ProgressReader::new(source, 1024 * 1024, move |done, total| {
        captured_events.lock().unwrap().push((done, total));
    });
    let mut buffer = [0_u8; 1024];

    while reader.read(&mut buffer).unwrap() != 0 {}

    let events = events.lock().unwrap();

    assert!(events.len() <= 101);
    assert_eq!(events.last(), Some(&(1024 * 1024, 1024 * 1024)));
}

#[test]
fn custom_header_validation_rejects_injection() {
    assert!(validate_upload_header("Authorization", "Bearer secret").is_ok());
    assert!(validate_upload_header("", "secret").is_err());
    assert!(validate_upload_header("Authorization\r\nInjected", "secret").is_err());
    assert!(validate_upload_header("Authorization", "secret\r\nInjected: yes").is_err());
    assert!(validate_upload_header("Content-Length", "1").is_err());
    assert!(validate_upload_header("Host", "elsewhere.example").is_err());
    assert!(validate_upload_header("Content-Type", "text/plain").is_err());
    assert!(validate_upload_header("Expect", "100-continue").is_err());
}

#[test]
fn generated_copies_never_replace_an_existing_capture() {
    let directory = tempfile::tempdir().unwrap();
    let mut payload = UploadPayload::from_bytes(
        vec![1, 2, 3],
        "sample.png".to_owned(),
        "image/png".to_owned(),
    );

    let first = payload.save_generated_copy(directory.path()).unwrap();
    let second = payload.save_generated_copy(directory.path()).unwrap();

    assert_eq!(first.file_name().unwrap(), "sample.png");
    assert_eq!(second.file_name().unwrap(), "sample-1.png");
    assert_eq!(std::fs::read(first).unwrap(), [1, 2, 3]);
    assert_eq!(std::fs::read(second).unwrap(), [1, 2, 3]);
}

#[test]
fn generated_reader_is_rewound_after_each_local_copy() {
    let directory = tempfile::tempdir().unwrap();
    let mut payload = UploadPayload::from_reader(
        Cursor::new(vec![4, 5, 6]),
        3,
        "sample.webp".to_owned(),
        "image/webp".to_owned(),
    );

    let first = payload.save_generated_copy(directory.path()).unwrap();
    let second = payload.save_generated_copy(directory.path()).unwrap();

    assert_eq!(std::fs::read(first).unwrap(), [4, 5, 6]);
    assert_eq!(std::fs::read(second).unwrap(), [4, 5, 6]);
}
