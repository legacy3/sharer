//! Persistent upload-history values stored by the `SQLite` backend.

use serde::{Deserialize, Serialize};

use crate::upload::UploadReceipt;

/// One successful upload, including its server-issued deletion capability.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HistoryEntry {
    pub link: String,
    pub delete_url: String,
    pub filename: String,
    pub size_bytes: u64,
    pub expires_at: String,
}

impl HistoryEntry {
    /// Move a server receipt into a persistent entry without cloning its strings.
    #[must_use]
    pub fn from_receipt(receipt: UploadReceipt) -> Self {
        Self {
            link: receipt.link,
            delete_url: receipt.delete_url,
            filename: receipt.original_name,
            size_bytes: receipt.size_bytes,
            expires_at: receipt.expires_at,
        }
    }
}
