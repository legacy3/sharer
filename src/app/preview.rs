//! Bounded local artifact previews, decoded by one worker outside the UI thread.
//!
//! Only PNG and JPEG are decoded, with independent limits on input, pixels and output.
//! Thumbnail sizes and channel lengths are bounded separately.
//! The decoder allocation budget provides additional best-effort protection.

use std::{
    io::{Cursor, Read as _, Seek as _},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
};

use anyhow::{Context as _, Result, ensure};
#[cfg(not(unix))]
use cap_std::fs::OpenOptions;
#[cfg(windows)]
use cap_std::fs::OpenOptionsExt as _;
use cap_std::{ambient_authority, fs::Dir};
use image::{
    DynamicImage, ImageDecoder as _, ImageFormat, ImageReader, Limits, RgbaImage,
    metadata::Orientation,
};

use crate::AppWindow;

const MAX_SOURCE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_DIMENSION: u32 = 8192;
const MAX_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_DECODED_BYTES: u64 = 64 * 1024 * 1024;
const DECODER_ALLOCATION_BUDGET: u64 = 128 * 1024 * 1024;
const THUMBNAIL_WIDTH: u32 = 640;
const THUMBNAIL_HEIGHT: u32 = 360;
const PREVIEW_EMPTY: i32 = 0;
const PREVIEW_LOADING: i32 = 1;
const PREVIEW_READY: i32 = 2;
const PREVIEW_UNSUPPORTED: i32 = 3;
const PREVIEW_UNAVAILABLE: i32 = 4;

#[derive(Debug)]
struct PreviewRequest {
    generation: u64,
    path: PathBuf,
}

#[derive(Debug)]
struct PreviewReply {
    request: PreviewRequest,
    artifact: PreviewArtifact,
}

#[derive(Debug)]
struct PreviewArtifact {
    state: i32,
    description: String,
    image: Option<RgbaImage>,
}

pub(super) struct Previewer {
    requests: SyncSender<PreviewRequest>,
    replies: Receiver<PreviewReply>,
    current_generation: Arc<AtomicU64>,
    generation: u64,
    path: PathBuf,
    pending: Option<PreviewRequest>,
}

impl Previewer {
    pub(super) fn new() -> Self {
        let (requests, request_rx) = mpsc::sync_channel(1);
        let (reply_tx, replies) = mpsc::sync_channel(1);
        let current_generation = Arc::new(AtomicU64::new(0));
        let worker_generation = Arc::clone(&current_generation);
        // A spawn failure closes request_rx; send_pending then exposes an unavailable preview.
        let _ = std::thread::Builder::new()
            .name("sharer-preview".to_owned())
            .spawn(move || decode_requests(&request_rx, &reply_tx, &worker_generation));

        Self {
            requests,
            replies,
            current_generation,
            generation: 0,
            path: PathBuf::new(),
            pending: None,
        }
    }

    pub(super) fn refresh(&mut self, window: &AppWindow) {
        let path = PathBuf::from(window.get_last_local_path().as_str());

        // A recreated window or explicitly cleared receipt starts at EMPTY even
        // if its path matches the previous receipt. Invalidate before draining.
        if path != self.path
            || (!path.as_os_str().is_empty()
                && window.get_artifact_preview_state() == PREVIEW_EMPTY)
        {
            self.replace_source(window, path);
        }

        match self.replies.try_recv() {
            Ok(reply) => {
                if self.accepts(&reply)
                    && Path::new(window.get_last_local_path().as_str()) == self.path
                {
                    apply_artifact(window, reply.artifact);
                }
            }

            Err(TryRecvError::Disconnected)
                if window.get_artifact_preview_state() == PREVIEW_LOADING =>
            {
                apply_artifact(window, unavailable("Preview could not be loaded."));
            }

            Err(_) => {}
        }

        self.send_pending(window);
    }

    pub(super) fn suspend(&mut self) {
        if !self.path.as_os_str().is_empty() || self.pending.is_some() {
            self.next_generation();
            self.path.clear();
            self.pending = None;
        }

        // Release completed thumbnails even when the native window was dropped.
        let _ = self.replies.try_recv();
    }

    fn replace_source(&mut self, window: &AppWindow, path: PathBuf) {
        self.next_generation();
        self.path = path;
        self.pending = None;
        clear(window);

        if self.path.as_os_str().is_empty() {
            return;
        }

        window.set_artifact_preview_state(PREVIEW_LOADING);
        window.set_artifact_name(
            self.path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .as_ref()
                .into(),
        );
        self.pending = Some(PreviewRequest {
            generation: self.generation,
            path: self.path.clone(),
        });
    }

    fn next_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.current_generation
            .store(self.generation, Ordering::Release);
    }

    fn accepts(&self, reply: &PreviewReply) -> bool {
        reply.request.generation == self.generation && reply.request.path == self.path
    }

    fn send_pending(&mut self, window: &AppWindow) {
        let Some(request) = self.pending.take() else {
            return;
        };

        match self.requests.try_send(request) {
            Ok(()) => {}

            Err(TrySendError::Full(request)) => self.pending = Some(request),

            Err(TrySendError::Disconnected(_)) => {
                apply_artifact(window, unavailable("Preview could not be loaded."));
            }
        }
    }
}

pub(super) fn clear(window: &AppWindow) {
    window.set_artifact_preview(slint::Image::default());
    window.set_artifact_preview_state(PREVIEW_EMPTY);
    window.set_artifact_name("".into());
    window.set_artifact_description("".into());
}

fn apply_artifact(window: &AppWindow, artifact: PreviewArtifact) {
    window.set_artifact_preview_state(artifact.state);
    window.set_artifact_description(artifact.description.into());

    if let Some(image) = artifact.image {
        let pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
            image.as_raw(),
            image.width(),
            image.height(),
        );

        window.set_artifact_preview(slint::Image::from_rgba8(pixels));
    } else {
        window.set_artifact_preview(slint::Image::default());
    }
}

fn decode_requests(
    requests: &Receiver<PreviewRequest>,
    replies: &SyncSender<PreviewReply>,
    current_generation: &AtomicU64,
) {
    while let Ok(mut request) = requests.recv() {
        if let Ok(newer) = requests.try_recv() {
            request = newer;
        }

        if current_generation.load(Ordering::Acquire) != request.generation {
            continue;
        }

        let artifact = load_artifact(&request.path).unwrap_or_else(|_| {
            unavailable("Preview could not be loaded. Try opening the local file.")
        });

        if current_generation.load(Ordering::Acquire) == request.generation
            && replies.send(PreviewReply { request, artifact }).is_err()
        {
            return;
        }
    }
}

fn load_artifact(path: &Path) -> Result<PreviewArtifact> {
    let filename = path.file_name().context("preview file name unavailable")?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let directory = Dir::open_ambient_dir(parent, ambient_authority())?;
    let metadata = directory.symlink_metadata(filename)?;

    ensure!(metadata.is_file(), "preview source is not a regular file");

    if metadata.len() > MAX_SOURCE_BYTES {
        return Ok(unavailable("File is larger than the 32 MiB preview limit."));
    }

    #[cfg(unix)]
    let mut file = std::fs::File::from(rustix::fs::openat(
        &directory,
        filename,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )?);
    #[cfg(not(unix))]
    let mut file = {
        let mut options = OpenOptions::new();

        options.read(true);
        #[cfg(windows)]
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
        directory.open_with(filename, &options)?.into_std()
    };
    let opened_metadata = file.metadata()?;

    ensure!(opened_metadata.is_file(), "preview source changed type");
    ensure!(
        opened_metadata.len() <= MAX_SOURCE_BYTES,
        "preview source grew"
    );
    let mut signature = [0_u8; 16];
    let count = usize::try_from(opened_metadata.len().min(16))?;

    file.read_exact(&mut signature[..count])?;

    if !matches!(
        image::guess_format(&signature[..count]),
        Ok(ImageFormat::Png | ImageFormat::Jpeg)
    ) {
        return Ok(unsupported(
            usize::try_from(opened_metadata.len()).unwrap_or(usize::MAX),
        ));
    }

    file.rewind()?;
    let mut bytes = Vec::new();

    file.take(MAX_SOURCE_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        u64::try_from(bytes.len()).unwrap_or(u64::MAX) <= MAX_SOURCE_BYTES,
        "preview source exceeded the read limit"
    );
    decode_artifact(&bytes)
}

fn decode_artifact(bytes: &[u8]) -> Result<PreviewArtifact> {
    let format = image::guess_format(bytes).ok();

    if !matches!(format, Some(ImageFormat::Png | ImageFormat::Jpeg)) {
        return Ok(unsupported(bytes.len()));
    }

    let format = format.context("preview format unavailable")?;
    let mut limits = Limits::default();

    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(DECODER_ALLOCATION_BUDGET);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);

    reader.limits(limits.clone());
    let mut decoder = reader.into_decoder()?;
    let (width, height) = decoder.dimensions();

    ensure!(
        u64::from(width) * u64::from(height) <= MAX_PIXELS,
        "image exceeds the preview pixel limit"
    );
    ensure!(
        decoder.total_bytes() <= MAX_DECODED_BYTES,
        "image exceeds the preview decoded-byte limit"
    );
    limits.reserve(decoder.total_bytes())?;
    decoder.set_limits(limits)?;
    let orientation = decoder.orientation()?;
    let image = DynamicImage::from_decoder(decoder)?;
    // Rotate only the small thumbnail: full-size rotation can duplicate a large
    // decoded buffer. The square intermediate preserves rotated portrait detail.
    let mut thumbnail = image.thumbnail(THUMBNAIL_WIDTH.min(width), THUMBNAIL_WIDTH.min(height));

    drop(image);
    thumbnail.apply_orientation(orientation);
    let (display_width, display_height) = match orientation {
        Orientation::Rotate90
        | Orientation::Rotate270
        | Orientation::Rotate90FlipH
        | Orientation::Rotate270FlipH => (height, width),
        _ => (width, height),
    };
    let thumbnail = thumbnail
        .thumbnail(
            THUMBNAIL_WIDTH.min(thumbnail.width()),
            THUMBNAIL_HEIGHT.min(thumbnail.height()),
        )
        .into_rgba8();
    let format_name = if format == ImageFormat::Png {
        "PNG"
    } else {
        "JPEG"
    };

    Ok(PreviewArtifact {
        state: PREVIEW_READY,
        description: format!(
            "{format_name} · {display_width} x {display_height} · {}",
            format_size(bytes.len())
        ),
        image: Some(thumbnail),
    })
}

fn unsupported(bytes: usize) -> PreviewArtifact {
    PreviewArtifact {
        state: PREVIEW_UNSUPPORTED,
        description: format!(
            "{} · Open the file to view its contents.",
            format_size(bytes)
        ),
        image: None,
    }
}

fn unavailable(message: &str) -> PreviewArtifact {
    PreviewArtifact {
        state: PREVIEW_UNAVAILABLE,
        description: message.to_owned(),
        image: None,
    }
}

fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{} KiB", bytes.div_ceil(1024))
    } else {
        format!("{} MiB", bytes.div_ceil(1024 * 1024))
    }
}

#[cfg(test)]
mod tests;
