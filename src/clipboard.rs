//! Cross-platform clipboard ingestion and result copying.

use std::{
    fmt,
    io::Cursor,
    path::Path,
    sync::{Arc, Mutex, PoisonError, Weak},
};

use anyhow::{Context, Result, bail, ensure};
use arboard::Clipboard;
use image::{DynamicImage, ImageFormat, RgbaImage};

use crate::upload::UploadPayload;

type SharedClipboard = Mutex<Option<Clipboard>>;

static SESSION: Mutex<Weak<SharedClipboard>> = Mutex::new(Weak::new());

/// Keep copied data available while the application is running.
///
/// Linux hosts clipboard data in its owning process.
/// Retain a session across clipboard calls, including while the window is hidden.
/// The first operation opens the clipboard lazily.
/// The last session releases ownership after any active operation finishes.
/// Release allows the platform's clipboard-manager handoff.
/// Persistence after process exit depends on the platform and clipboard manager.
#[must_use]
pub struct ClipboardSession {
    _owner: Arc<SharedClipboard>,
}

impl ClipboardSession {
    /// Start or share the process's clipboard lifetime.
    pub fn new() -> Self {
        let mut session = SESSION.lock().unwrap_or_else(PoisonError::into_inner);

        Self {
            _owner: shared_owner(&mut session),
        }
    }
}

impl fmt::Debug for ClipboardSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClipboardSession")
            .finish_non_exhaustive()
    }
}

impl Default for ClipboardSession {
    fn default() -> Self {
        Self::new()
    }
}

fn shared_owner(session: &mut Weak<SharedClipboard>) -> Arc<SharedClipboard> {
    let owner = session
        .upgrade()
        .unwrap_or_else(|| Arc::new(Mutex::new(None)));

    *session = Arc::downgrade(&owner);

    owner
}

fn with_clipboard<T>(operation: impl FnOnce(&mut Clipboard) -> Result<T>) -> Result<T> {
    let owner = {
        let mut session = SESSION
            .try_lock()
            .map_err(|error| anyhow::anyhow!("clipboard is busy or unavailable: {error}"))?;

        shared_owner(&mut session)
    };
    // Clipboard calls can originate from the UI or the upload worker. Do not
    // make the UI wait for another operation, and serialize Windows access.
    let mut clipboard = owner
        .try_lock()
        .map_err(|error| anyhow::anyhow!("clipboard is busy or unavailable: {error}"))?;

    if clipboard.is_none() {
        *clipboard = Some(Clipboard::new().context("clipboard is unavailable")?);
    }

    operation(clipboard.as_mut().context("clipboard is unavailable")?)
}

/// Read the clipboard, preferring copied files, then images, then text.
///
/// # Errors
///
/// Returns an error when the clipboard is unavailable or contains no supported data.
pub fn read_payload() -> Result<UploadPayload> {
    with_clipboard(read_payload_from)
}

fn read_payload_from(clipboard: &mut Clipboard) -> Result<UploadPayload> {
    if let Ok(files) = clipboard.get().file_list()
        && !files.is_empty()
    {
        ensure!(
            files.len() == 1,
            "clipboard contains multiple files; select one file to upload"
        );
        let path = &files[0];

        ensure!(path.is_file(), "clipboard path is not a regular file");

        return UploadPayload::from_path(path);
    }

    if let Ok(image) = clipboard.get_image() {
        let width = u32::try_from(image.width).context("clipboard image is too wide")?;
        let height = u32::try_from(image.height).context("clipboard image is too tall")?;
        let rgba = RgbaImage::from_raw(width, height, image.bytes.into_owned())
            .context("clipboard returned an invalid RGBA image")?;
        let mut bytes = Vec::new();

        DynamicImage::ImageRgba8(rgba)
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .context("failed to encode clipboard image")?;

        return Ok(UploadPayload::from_bytes(
            bytes,
            "clipboard.png".to_owned(),
            "image/png".to_owned(),
        ));
    }

    if let Ok(text) = clipboard.get_text()
        && !text.is_empty()
    {
        return Ok(UploadPayload::from_bytes(
            text.into_bytes(),
            "clipboard.txt".to_owned(),
            "text/plain; charset=utf-8".to_owned(),
        ));
    }

    bail!("clipboard contains no file, image, or text")
}

/// Replace clipboard text with the returned upload URL.
/// Keep a [`ClipboardSession`] alive to retain ownership between operations.
///
/// # Errors
///
/// Returns an error if the clipboard cannot be opened or changed.
pub fn copy_link(link: &str) -> Result<()> {
    with_clipboard(|clipboard| {
        clipboard
            .set_text(link)
            .context("failed to copy upload link")
    })
}

/// Put a local file on the clipboard so it can be pasted into another application.
/// Keep a [`ClipboardSession`] alive to retain ownership between operations.
///
/// # Errors
///
/// Returns an error when the path is not a file or the clipboard cannot be changed.
pub fn copy_file(path: &Path) -> Result<()> {
    ensure!(path.is_file(), "local capture no longer exists");

    with_clipboard(|clipboard| {
        clipboard
            .set()
            .file_list(&[path])
            .context("failed to copy local file")
    })
}
