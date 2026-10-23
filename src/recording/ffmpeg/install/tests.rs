use std::io::Cursor;

use super::*;

fn archive(entries: &[(&str, &[u8])]) -> Cursor<Vec<u8>> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));

    for (name, contents) in entries {
        writer
            .start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(contents).unwrap();
    }

    writer.finish().unwrap()
}

#[test]
fn download_source_matches_each_shipped_platform_and_architecture() {
    let windows = source_for("windows", "x86_64").unwrap();

    assert!(windows.archive_url.starts_with("https://www.gyan.dev/"));

    for (os, architecture, path) in [
        ("linux", "x86_64", "/linux/amd64/"),
        ("linux", "aarch64", "/linux/arm64/"),
        ("macos", "x86_64", "/macos/amd64/"),
        ("macos", "aarch64", "/macos/arm64/"),
    ] {
        let source = source_for(os, architecture).unwrap();

        assert!(
            source
                .archive_url
                .starts_with("https://ffmpeg.martin-riedl.de/")
        );
        assert!(source.archive_url.contains(path));
        assert!(source.archive_url.ends_with("_9.0.2/ffmpeg.zip"));
        assert_eq!(source.sha256.len(), 64);
        assert!(source.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    assert!(source_for("windows", "aarch64").is_err());
    assert!(source_for("linux", "arm").is_err());
}

#[test]
fn streaming_hash_and_cancellation_do_not_publish_a_partial_download() {
    let cancelled = AtomicBool::new(false);
    let contents = b"abc";
    let mut output = Vec::new();
    let mut writer = DownloadWriter::new(&mut output, Some(3));

    writer.push(contents, &cancelled).unwrap();
    let checksum = writer.finish().unwrap();

    assert_eq!(
        checksum,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(output, contents);
    let mut writer = DownloadWriter::new(Vec::new(), Some(4));

    writer.push(contents, &cancelled).unwrap();
    assert!(writer.finish().is_err());
    cancelled.store(true, Ordering::Release);
    let mut output = Vec::new();

    assert!(
        DownloadWriter::new(&mut output, Some(3))
            .push(contents, &cancelled)
            .is_err()
    );
    assert!(output.is_empty());
    let mut writer = DownloadWriter::new(Vec::new(), None);

    writer.total = MAX_DOWNLOAD_BYTES;
    assert!(writer.push(b"x", &AtomicBool::new(false)).is_err());
    assert!(writer.output.is_empty());
}

#[test]
fn cancellation_interrupts_a_stalled_network_operation() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let cancelled = AtomicBool::new(false);

    runtime.block_on(async {
        let cancel = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancelled.store(true, Ordering::Release);
        };
        let operation = async {
            let result = tokio::time::timeout(
                Duration::from_secs(1),
                cancellable(std::future::pending::<()>(), &cancelled),
            )
            .await
            .unwrap();

            assert!(result.is_err());
        };

        futures_util::future::join(cancel, operation).await;
    });
}

#[test]
fn only_the_encoder_is_extracted_and_path_traversal_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let binary = format!("build/bin/{EXECUTABLE}");
    let contents = b"test encoder";
    let mut candidate = extract_executable(
        archive(&[
            ("../../outside.txt", b"ignored"),
            ("build/bin/ffprobe", b"ignored"),
            (&binary, contents),
        ]),
        directory.path(),
        &AtomicBool::new(false),
    )
    .unwrap();
    let mut actual = Vec::new();

    candidate.seek(SeekFrom::Start(0)).unwrap();
    candidate.read_to_end(&mut actual).unwrap();
    assert_eq!(actual, contents);
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    assert!(
        extract_executable(
            archive(&[(&format!("../../{EXECUTABLE}"), contents)]),
            directory.path(),
            &AtomicBool::new(false)
        )
        .is_err()
    );
}

#[test]
fn duplicate_encoders_and_cancelled_extraction_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let other = format!("other/{EXECUTABLE}");

    assert!(
        extract_executable(
            archive(&[(EXECUTABLE, b"one"), (&other, b"two")]),
            directory.path(),
            &AtomicBool::new(false)
        )
        .is_err()
    );
    assert!(
        extract_executable(
            archive(&[(EXECUTABLE, b"one")]),
            directory.path(),
            &AtomicBool::new(true)
        )
        .is_err()
    );
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
#[ignore = "downloads and verifies the platform FFmpeg build in an isolated temporary folder"]
fn managed_download_verifies_the_release_and_installs_a_working_encoder() {
    let directory = tempfile::tempdir().unwrap();
    let executable = install_into(
        directory.path(),
        &source().unwrap(),
        false,
        &AtomicBool::new(false),
        |_, _| {},
    )
    .unwrap();

    assert_eq!(executable.file_name().unwrap(), EXECUTABLE);
    assert!(executable.is_file());
    assert!(directory.path().join("SOURCE.txt").is_file());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
    validate_encoder(&executable, &AtomicBool::new(false)).unwrap();
}

#[test]
fn cancelled_download_does_not_wait_for_a_running_blocking_resolver() {
    let (started_sender, started_receiver) = std::sync::mpsc::channel();
    let (release_sender, release_receiver) = std::sync::mpsc::channel();
    let (result_sender, result_receiver) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = run_download(async move {
            let _resolver = tokio::task::spawn_blocking(move || {
                started_sender.send(()).unwrap();
                let _ = release_receiver.recv_timeout(Duration::from_secs(5));
            });

            started_receiver
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
            cancellable(std::future::pending::<()>(), &AtomicBool::new(true)).await
        });

        result_sender.send(result).unwrap();
    });
    let result = result_receiver.recv_timeout(Duration::from_secs(2));

    // Release the synthetic DNS task even when a regression times out, so the
    // test reports a failure instead of leaking a permanently blocked thread.
    let _ = release_sender.send(());

    worker.join().unwrap();
    assert!(
        result
            .expect("cancellation must return before DNS completes")
            .is_err()
    );
}
