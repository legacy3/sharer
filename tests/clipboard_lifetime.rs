//! Cross-process Linux clipboard regression; run only inside an isolated X11 fixture.

#![cfg(target_os = "linux")]

use std::{path::Path, process::Command};

use arboard::Clipboard;
use sharer::clipboard::{ClipboardSession, copy_file, copy_link, read_payload};

const READER_MODE: &str = "SHARER_CLIPBOARD_TEST_READER";
const READER_VALUE: &str = "SHARER_CLIPBOARD_TEST_VALUE";

fn assert_external_clipboard(kind: &str, expected: &str) {
    let result = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "copied_values_survive_return_and_overwrite",
            "--ignored",
            "--nocapture",
        ])
        .env(READER_MODE, kind)
        .env(READER_VALUE, expected)
        .output()
        .unwrap();

    assert!(
        result.status.success(),
        "independent clipboard reader failed: {}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

fn read_in_child(kind: &str) {
    let expected = std::env::var(READER_VALUE).unwrap();
    let mut clipboard = Clipboard::new().unwrap();

    match kind {
        "text" => assert_eq!(clipboard.get_text().unwrap(), expected),

        "file" => assert_eq!(
            clipboard.get().file_list().unwrap(),
            [Path::new(&expected).canonicalize().unwrap()]
        ),

        _ => panic!("unexpected clipboard reader mode"),
    }
}

#[test]
#[ignore = "requires an isolated X11 display with no clipboard manager"]
fn copied_values_survive_return_and_overwrite() {
    if let Ok(kind) = std::env::var(READER_MODE) {
        read_in_child(&kind);

        return;
    }

    let session = ClipboardSession::new();
    let other_session = ClipboardSession::new();
    let first = "https://files.example.invalid/first";

    copy_link(first).unwrap();
    assert_external_clipboard("text", first);
    assert_eq!(read_payload().unwrap().filename, "clipboard.txt");
    // Dropping one of two owners must not erase another session's selection.
    drop(session);
    assert_external_clipboard("text", first);

    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("clipboard fixture #1.txt");

    std::fs::write(&file, b"dummy clipboard fixture").unwrap();
    copy_file(&file).unwrap();
    assert_external_clipboard("file", file.to_str().unwrap());
    assert_eq!(read_payload().unwrap().filename, "clipboard fixture #1.txt");

    let second = "https://files.example.invalid/second";

    // Automatic uploads copy from a worker; its thread must not own the lifetime.
    std::thread::spawn(move || copy_link(second))
        .join()
        .unwrap()
        .unwrap();
    assert_external_clipboard("text", second);
    assert!(copy_file(&directory.path().join("missing.txt")).is_err());
    assert_external_clipboard("text", second);
    drop(other_session);
}
