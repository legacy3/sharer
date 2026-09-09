//! Region recording into a self-contained animated WebP.

use std::{
    sync::mpsc::{Receiver, RecvTimeoutError},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use sharer::{
    capture::{DesktopRegion, RegionCapturer},
    config::CaptureResolution,
    upload::UploadPayload,
    validate_recording_fps,
};
use webp_animation::{Encoder, EncoderOptions, EncodingConfig};

#[derive(Clone, Copy, Debug)]
pub(super) struct RecordingSettings {
    pub(super) frames_per_second: u32,
    pub(super) max_seconds: u32,
    pub(super) capture_resolution: CaptureResolution,
}

pub(super) fn capture_region(
    region: DesktopRegion,
    stop: &Receiver<()>,
    filename_stem: &str,
    settings: RecordingSettings,
    now: fn() -> Instant,
) -> Result<UploadPayload> {
    let RecordingSettings {
        frames_per_second,
        max_seconds,
        capture_resolution,
    } = settings;

    validate_recording_fps(frames_per_second)?;
    sharer::validate_recording_seconds(max_seconds)?;

    let frame_interval = Duration::from_secs_f64(1.0 / f64::from(frames_per_second));
    let capturer = RegionCapturer::new(region, capture_resolution)?;
    let dimensions = capturer.dimensions();

    anyhow::ensure!(
        dimensions.0 <= 16_383 && dimensions.1 <= 16_383,
        "recording region exceeds WebP's 16383-pixel dimension limit"
    );
    let options = EncoderOptions {
        encoding_config: Some(EncodingConfig::new_lossy(80.0)),
        ..EncoderOptions::default()
    };
    let mut encoder = Encoder::new_with_options(dimensions, options)
        .context("failed to initialize the WebP recorder")?;
    let started = now();
    let deadline = started
        .checked_add(Duration::from_secs(u64::from(max_seconds)))
        .context("recording duration is too long")?;
    let mut next_frame = started;
    let mut last_timestamp: Option<i32> = None;

    loop {
        let current_time = now();

        if last_timestamp.is_some() && current_time >= deadline {
            break;
        }

        let elapsed_timestamp =
            duration_timestamp(current_time.saturating_duration_since(started))?;
        let timestamp = match last_timestamp {
            Some(previous) => previous
                .checked_add(1)
                .context("recording duration is too long")?
                .max(elapsed_timestamp),

            None => elapsed_timestamp,
        };
        let image = capturer
            .capture_rgba()
            .context("failed to capture a recording frame")?;

        anyhow::ensure!(
            image.len()
                == usize::try_from(dimensions.0)?
                    .saturating_mul(usize::try_from(dimensions.1)?)
                    .saturating_mul(4),
            "screen capture returned an unexpected recording size"
        );
        encoder
            .add_frame(&image, timestamp)
            .context("failed to encode a recording frame")?;
        last_timestamp = Some(timestamp);

        next_frame = next_frame
            .checked_add(frame_interval)
            .context("recording duration is too long")?;
        let current_time = now();

        if next_frame < current_time {
            next_frame = current_time;
        }

        let next_wake = next_frame.min(deadline);

        match stop.recv_timeout(next_wake.saturating_duration_since(current_time)) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }

    let final_timestamp = duration_timestamp(now().saturating_duration_since(started))?.max(
        last_timestamp
            .context("recording produced no frames")?
            .checked_add(1)
            .context("recording duration is too long")?,
    );
    let data = encoder
        .finalize(final_timestamp)
        .context("failed to finish the WebP recording")?;
    let length = u64::try_from(data.len()).context("recording is too large")?;

    Ok(UploadPayload::from_reader(
        std::io::Cursor::new(data),
        length,
        format!("{filename_stem}-recording.webp"),
        "image/webp".to_owned(),
    ))
}

fn duration_timestamp(elapsed: Duration) -> Result<i32> {
    let milliseconds = elapsed.as_millis();

    i32::try_from(milliseconds).context("recording duration is too long")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::duration_timestamp;

    #[test]
    fn timestamps_use_real_elapsed_milliseconds() {
        assert_eq!(duration_timestamp(Duration::ZERO).unwrap(), 0);
        assert_eq!(
            duration_timestamp(Duration::from_micros(16_999)).unwrap(),
            16
        );
        assert_eq!(duration_timestamp(Duration::from_secs(1)).unwrap(), 1_000);
    }
}
