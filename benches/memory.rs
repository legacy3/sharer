use divan::black_box;
use sharer::{
    config::AppConfig,
    history::HistoryEntry,
    upload::{UploadPayload, UploadReceipt, validate_uploader_url},
};

fn main() {
    divan::main();
}

#[divan::bench]
fn encode_configuration() -> Vec<u8> {
    serde_json::to_vec(black_box(&AppConfig::default())).unwrap()
}

#[divan::bench]
fn validate_endpoint() {
    validate_uploader_url(black_box("https://uploads.example/upload?client=desktop")).unwrap();
}

#[divan::bench(args = [0, 64 * 1024, 4 * 1024 * 1024])]
fn prepare_generated_payload(size: usize) -> UploadPayload {
    UploadPayload::from_bytes(
        black_box(vec![0; size]),
        "capture.png".to_owned(),
        "image/png".to_owned(),
    )
}

#[divan::bench]
fn move_history_entry() -> HistoryEntry {
    let receipt = UploadReceipt {
        original_name: "capture.png".to_owned(),
        size_bytes: 4096,
        link: "https://files.example/capture.png".to_owned(),
        delete_url: "https://files.example/delete/capture/token".to_owned(),
        expires_at: "2026-09-09T00:00:00Z".to_owned(),
    };

    HistoryEntry::from_receipt(black_box(receipt))
}
