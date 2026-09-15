//! Persistent capture-history values stored by the `SQLite` backend.

use serde::{Deserialize, Serialize};

use crate::upload::UploadReceipt;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HistorySortColumn {
    Filename,
    #[default]
    CreatedAt,
    Size,
    ExpiresAt,
}

impl HistorySortColumn {
    /// Convert a desktop table-column index to a sortable history field.
    #[must_use]
    pub const fn from_index(index: i32) -> Option<Self> {
        match index {
            0 => Some(Self::Filename),
            1 => Some(Self::CreatedAt),
            2 => Some(Self::Size),
            3 => Some(Self::ExpiresAt),
            _ => None,
        }
    }

    /// Convert a sortable history field to its desktop table-column index.
    #[must_use]
    pub const fn index(self) -> i32 {
        match self {
            Self::Filename => 0,
            Self::CreatedAt => 1,
            Self::Size => 2,
            Self::ExpiresAt => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SortDirection {
    Ascending,
    #[default]
    Descending,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HistorySort {
    pub column: HistorySortColumn,
    pub direction: SortDirection,
}

/// One successful local capture or upload and its available actions.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HistoryEntry {
    pub id: i64,
    #[serde(default)]
    pub created_at: i64,
    pub link: String,
    pub delete_url: String,
    pub local_path: String,
    pub filename: String,
    pub size_bytes: u64,
    pub expires_at: String,
}

impl HistoryEntry {
    /// Move a server receipt into a persistent entry without cloning its strings.
    #[must_use]
    pub fn from_receipt(receipt: UploadReceipt, local_path: Option<&std::path::Path>) -> Self {
        Self {
            id: 0,
            created_at: chrono::Utc::now().timestamp(),
            link: receipt.link,
            delete_url: receipt.delete_url,
            local_path: local_path
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
            filename: receipt.original_name,
            size_bytes: receipt.size_bytes,
            expires_at: receipt.expires_at,
        }
    }

    /// Create a history entry for a capture that remains only on this computer.
    #[must_use]
    pub fn local(filename: String, size_bytes: u64, path: &std::path::Path) -> Self {
        Self {
            id: 0,
            created_at: chrono::Utc::now().timestamp(),
            link: String::new(),
            delete_url: String::new(),
            local_path: path.to_string_lossy().into_owned(),
            filename,
            size_bytes,
            expires_at: String::new(),
        }
    }
}
