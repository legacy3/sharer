use allocation_counter::measure;
use sharer::{
    history::HistoryEntry,
    upload::{UploadClient, UploadPayload, UploadReceipt, UploadRoute},
    validate_lifetime,
};
use std::{
    fs::File,
    io::{Cursor, Read as _, Seek as _, SeekFrom, Write as _},
    net::{TcpListener, TcpStream},
};

const STREAMED_FILE_SIZE: u64 = 16 * 1024 * 1024;

#[test]
fn lifetime_validation_does_not_allocate() {
    let allocations = measure(|| {
        for seconds in [1, 60, 3600, 86_400, 604_800] {
            std::hint::black_box(validate_lifetime(seconds)).unwrap();
        }
    });

    assert_eq!(allocations.count_total, 0);
    assert_eq!(allocations.bytes_total, 0);
}

#[test]
fn wrapping_owned_capture_bytes_does_not_allocate() {
    let bytes = vec![0_u8; 4 * 1024 * 1024];
    let filename = "capture.png".to_owned();
    let mime = "image/png".to_owned();
    let mut payload = None;
    let allocations = measure(|| {
        payload = Some(UploadPayload::from_bytes(bytes, filename, mime));
    });

    assert_eq!(allocations.count_total, 0);
    assert_eq!(allocations.bytes_total, 0);
    drop(payload);
}

#[test]
fn wrapping_a_seekable_recording_does_not_copy_its_buffer() {
    let bytes = vec![0_u8; 4 * 1024 * 1024];
    let filename = "recording.webp".to_owned();
    let mime = "image/webp".to_owned();
    let mut payload = None;
    let allocations = measure(|| {
        payload = Some(UploadPayload::from_reader(
            Cursor::new(bytes),
            4 * 1024 * 1024,
            filename,
            mime,
        ));
    });

    assert_eq!(allocations.count_total, 1);
    assert!(
        allocations.bytes_total < 128,
        "wrapping a 4 MiB recording allocated {} bytes",
        allocations.bytes_total
    );
    drop(payload);
}

#[test]
fn moving_a_receipt_into_history_does_not_allocate() {
    let receipt = UploadReceipt {
        original_name: "capture.png".to_owned(),
        size_bytes: 4096,
        link: "https://files.example/capture.png".to_owned(),
        delete_url: "https://files.example/delete/capture/token".to_owned(),
        expires_at: "2026-09-09T00:00:00Z".to_owned(),
    };
    let mut entry = None;
    let allocations = measure(|| {
        entry = Some(HistoryEntry::from_receipt(receipt));
    });

    assert_eq!(allocations.count_total, 0);
    assert_eq!(allocations.bytes_total, 0);
    drop(entry);
}

#[test]
fn preparing_a_large_file_does_not_buffer_its_contents() {
    const FILE_SIZE: u64 = 64 * 1024 * 1024;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("large-upload.bin");
    let file = File::create(&path).unwrap();

    file.set_len(FILE_SIZE).unwrap();
    drop(file);

    let mut payload = None;
    let allocations = measure(|| {
        payload = Some(UploadPayload::from_path(&path).unwrap());
    });

    assert!(
        allocations.bytes_total < 16 * 1024,
        "preparing a sparse {FILE_SIZE}-byte file allocated {} bytes",
        allocations.bytes_total
    );
    drop(payload);
}

#[test]
fn inspecting_a_large_metadata_free_image_does_not_buffer_it() {
    const DATA_SIZE: u32 = 32 * 1024 * 1024;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("large-image.png");
    let mut file = File::create(&path).unwrap();

    file.write_all(b"\x89PNG\r\n\x1a\n").unwrap();
    file.write_all(&DATA_SIZE.to_be_bytes()).unwrap();
    file.write_all(b"IDAT").unwrap();
    file.seek(SeekFrom::Current(i64::from(DATA_SIZE))).unwrap();
    file.write_all(&[0; 4]).unwrap();
    file.write_all(&0_u32.to_be_bytes()).unwrap();
    file.write_all(b"IEND").unwrap();
    file.write_all(&[0; 4]).unwrap();
    drop(file);

    let mut payload = UploadPayload::from_path(&path).unwrap();
    let allocations = measure(|| payload.remove_exif().unwrap());

    assert!(
        allocations.bytes_total < 4 * 1024,
        "inspecting a metadata-free image allocated {} bytes",
        allocations.bytes_total
    );
}

#[test]
fn stripping_in_memory_exif_reuses_the_capture_buffer() {
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();

    png.extend_from_slice(&4_u32.to_be_bytes());
    png.extend_from_slice(b"eXIf");
    png.extend_from_slice(b"meta");
    png.extend_from_slice(&[0; 4]);
    png.extend_from_slice(&0_u32.to_be_bytes());
    png.extend_from_slice(b"IEND");
    png.extend_from_slice(&[0; 4]);

    let mut payload =
        UploadPayload::from_bytes(png, "capture.png".to_owned(), "image/png".to_owned());
    let allocations = measure(|| payload.remove_exif().unwrap());

    assert_eq!(allocations.count_total, 0);
    assert_eq!(allocations.bytes_total, 0);
}

#[test]
fn streaming_a_large_file_has_a_size_independent_peak_allocation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("streamed-upload.bin");
    let file = File::create(&path).unwrap();

    file.set_len(STREAMED_FILE_SIZE).unwrap();
    drop(file);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || serve_one_upload(listener.accept().unwrap().0));
    let endpoint = format!("http://{address}/upload");
    let client = UploadClient::new(&endpoint, &UploadRoute::Direct, None).unwrap();
    let payload = UploadPayload::from_path(&path).unwrap();
    let mut receipt = None;
    let allocations = measure(|| {
        receipt = Some(client.upload(payload, 60).unwrap());
    });

    server.join().unwrap();
    assert_eq!(receipt.unwrap().size_bytes, STREAMED_FILE_SIZE);
    assert!(
        allocations.bytes_max < 2 * 1024 * 1024,
        "streaming a {STREAMED_FILE_SIZE}-byte file held {} heap bytes at peak",
        allocations.bytes_max
    );
}

fn serve_one_upload(mut stream: TcpStream) {
    let mut received = Vec::with_capacity(8 * 1024);
    let mut buffer = vec![0_u8; 64 * 1024];
    let (header_end, content_length) = loop {
        let count = stream.read(&mut buffer).unwrap();

        assert!(count > 0, "client closed before sending HTTP headers");
        received.extend_from_slice(&buffer[..count]);

        if let Some(header_end) = received.windows(4).position(|window| window == b"\r\n\r\n") {
            let header_end = header_end + 4;
            let headers = String::from_utf8_lossy(&received[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(str::trim)
                        .map(str::parse::<usize>)
                })
                .unwrap()
                .unwrap();

            break (header_end, content_length);
        }
    };
    let mut body_bytes = received.len() - header_end;

    while body_bytes < content_length {
        let count = stream.read(&mut buffer).unwrap();

        assert!(
            count > 0,
            "client closed before finishing the multipart body"
        );
        body_bytes += count;
    }

    let response_body = format!(
        "{{\"data\":{{\"originalName\":\"streamed-upload.bin\",\"size\":{STREAMED_FILE_SIZE},\"link\":\"https://files.example/file\",\"deleteLink\":\"https://files.example/delete/token\",\"expiresAt\":\"2026-09-09T00:00:00Z\"}}}}"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
        response_body.len()
    );

    stream.write_all(response.as_bytes()).unwrap();
}
