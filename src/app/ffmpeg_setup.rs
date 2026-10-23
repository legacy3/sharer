//! User-requested setup of the optional recording encoder.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::Result;
use slint::ComponentHandle as _;

use crate::{AppWindow, recording::ffmpeg};

pub(super) fn show_missing(window: &AppWindow) {
    show_missing_with_source(window, ffmpeg::install::source());
}

fn show_missing_with_source(window: &AppWindow, source: Result<ffmpeg::install::DownloadSource>) {
    let available = source.is_ok();
    let message = match source {
        Ok(source) => format!(
            "MP4 recording needs FFmpeg. Download it from {} into ShareR's own folder? No PATH changes or administrator access are needed. To use WebP instead, cancel and turn off MP4 recording.",
            source.provider
        ),

        Err(error) => format!(
            "{error}. You can also use animated WebP without FFmpeg: cancel and turn off MP4 recording in Settings → Capture."
        ),
    };

    window.set_busy(true);
    window.set_ffmpeg_download_available(available);
    window.set_ffmpeg_cancelling(false);
    window.set_ffmpeg_setup_failed(false);
    window.set_ffmpeg_setup_message(message.into());
    window.set_ffmpeg_setup_visible(true);
    window.set_status_text("FFmpeg required".into());
    window.set_status_detail(
        if available {
            "Download FFmpeg or turn off MP4 to use WebP"
        } else {
            "Install FFmpeg manually or choose WebP in Settings → Capture"
        }
        .into(),
    );
    let _ = window.show();
}

pub(super) fn wire(window: &AppWindow, require_proxy: bool) {
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancellation = Arc::clone(&cancelled);
    let weak = window.as_weak();

    window.on_cancel_ffmpeg_setup(move || {
        if let Some(window) = weak.upgrade() {
            if window.get_ffmpeg_downloading() {
                if !window.get_ffmpeg_cancelling() {
                    window.set_ffmpeg_cancelling(true);
                    cancellation.store(true, Ordering::Release);
                    window.set_ffmpeg_setup_message(
                        "Cancelling setup and removing temporary files...".into(),
                    );
                }
            } else {
                window.set_ffmpeg_setup_visible(false);
                window.set_busy(false);
                window.set_status_text("FFmpeg setup cancelled".into());
                window.set_status_detail("Turn off MP4 recording to use WebP".into());
            }
        }
    });
    let weak = window.as_weak();

    window.on_download_ffmpeg(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        if window.get_ffmpeg_downloading()
            || !window.get_ffmpeg_setup_visible()
            || !window.get_ffmpeg_download_available()
        {
            return;
        }

        cancelled.store(false, Ordering::Release);
        window.set_busy(true);
        window.set_ffmpeg_cancelling(false);
        window.set_ffmpeg_setup_failed(false);
        window.set_ffmpeg_downloading(true);
        window.set_ffmpeg_download_progress(0.0);
        window.set_ffmpeg_setup_message("Preparing the FFmpeg download...".into());
        let proxy_required = require_proxy || window.get_require_tor();
        let cancellation = Arc::clone(&cancelled);
        let result_window = window.as_weak();
        let progress_window = window.as_weak();
        let spawn_result = std::thread::Builder::new()
            .name("sharer-ffmpeg-download".into())
            .spawn(move || {
                let mut previous_progress = None;
                let result =
                    ffmpeg::install::install(proxy_required, &cancellation, |downloaded, total| {
                        let percent = total
                            .filter(|total| *total > 0)
                            .map(|total| (downloaded * 100 / total).min(100));
                        let step = percent.unwrap_or(downloaded / (1024 * 1024));

                        if previous_progress == Some(step) {
                            return;
                        }

                        previous_progress = Some(step);
                        let progress = u8::try_from(percent.unwrap_or(0)).unwrap_or(100);
                        let message = if progress == 100 {
                            "Verifying and preparing FFmpeg...".to_owned()
                        } else {
                            format!(
                                "Downloading FFmpeg... {} MiB received",
                                downloaded / (1024 * 1024)
                            )
                        };
                        let _ = progress_window.upgrade_in_event_loop(move |window| {
                            show_progress(&window, f32::from(progress) / 100.0, message);
                        });
                    });
                let was_cancelled = cancellation.load(Ordering::Acquire);

                let _ = result_window
                    .upgrade_in_event_loop(move |window| finish(&window, was_cancelled, result));
            });

        if let Err(error) = spawn_result {
            finish(&window, false, Err(error.into()));
        }
    });
}

fn show_progress(window: &AppWindow, progress: f32, message: String) {
    if window.get_ffmpeg_setup_visible()
        && window.get_ffmpeg_downloading()
        && !window.get_ffmpeg_cancelling()
    {
        window.set_ffmpeg_download_progress(progress);
        window.set_ffmpeg_setup_message(message.into());
    }
}

fn finish(window: &AppWindow, cancelled: bool, result: Result<PathBuf>) {
    window.set_ffmpeg_downloading(false);
    window.set_ffmpeg_cancelling(false);
    window.set_ffmpeg_download_progress(0.0);

    match result {
        // Publishing the verified executable is the commit point. A late Cancel
        // must not claim that an already installed encoder was rolled back.
        Ok(_) => {
            window.set_ffmpeg_setup_visible(false);
            window.set_ffmpeg_setup_failed(false);
            window.set_busy(false);
            window.set_status_text("FFmpeg ready".into());
            window.set_status_detail("Press Record to start an MP4 recording".into());
        }

        Err(_) if cancelled => {
            window.set_ffmpeg_setup_visible(false);
            window.set_ffmpeg_setup_failed(false);
            window.set_busy(false);
            window.set_status_text("FFmpeg setup cancelled".into());
            window.set_status_detail("You can retry setup or use WebP".into());
        }

        Err(error) => {
            window.set_ffmpeg_setup_failed(true);
            window.set_ffmpeg_setup_message(format!("{error:#}. Retry the download, or cancel and turn off MP4 recording in Settings → Capture to use WebP.").into());
            window.set_status_text("FFmpeg setup failed".into());
            window.set_status_detail("Retry the download or cancel setup".into());
        }
    }
}

#[cfg(test)]
mod tests;
