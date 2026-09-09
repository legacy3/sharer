//! Core functionality for the `ShareR` desktop uploader.

#[cfg(feature = "desktop")]
pub mod capture;
pub mod clipboard;
pub mod config;
pub mod history;
pub mod proxy;
pub mod storage;
pub mod upload;

/// The largest supported upload lifetime: one week in seconds.
pub const MAX_LIFETIME_SECONDS: u32 = 7 * 24 * 60 * 60;
/// Slowest supported region recording frame rate.
pub const MIN_RECORDING_FPS: u32 = 1;
/// Fastest supported region recording frame rate.
pub const MAX_RECORDING_FPS: u32 = 60;
/// Longest supported region recording, preventing unattended memory growth.
pub const MAX_RECORDING_SECONDS: u32 = 3_600;
/// Default maximum duration of one region recording.
pub const DEFAULT_RECORDING_SECONDS: u32 = 300;
/// Largest history page returned by one database query.
pub const MAX_HISTORY_PAGE_SIZE: usize = 1_000;

/// Validate a remote file lifetime.
///
/// # Errors
///
/// Returns an error when the lifetime is zero or longer than one week.
pub fn validate_lifetime(seconds: u32) -> Result<u32, LifetimeError> {
    if !(1..=MAX_LIFETIME_SECONDS).contains(&seconds) {
        return Err(LifetimeError(seconds));
    }

    Ok(seconds)
}

/// An invalid requested lifetime.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("lifetime must be between 1 and {MAX_LIFETIME_SECONDS} seconds (one week), got {0}")]
pub struct LifetimeError(pub u32);

/// Validate a region recording frame rate.
///
/// # Errors
///
/// Returns an error when the rate is outside 1 through 60 frames per second.
pub fn validate_recording_fps(frames_per_second: u32) -> Result<u32, RecordingFpsError> {
    if !(MIN_RECORDING_FPS..=MAX_RECORDING_FPS).contains(&frames_per_second) {
        return Err(RecordingFpsError(frames_per_second));
    }

    Ok(frames_per_second)
}

/// Validate the configured recording duration bound.
///
/// # Errors
///
/// Returns an error when the limit is zero or longer than one hour.
pub fn validate_recording_seconds(seconds: u32) -> Result<u32, RecordingDurationError> {
    if (1..=MAX_RECORDING_SECONDS).contains(&seconds) {
        Ok(seconds)
    } else {
        Err(RecordingDurationError(seconds))
    }
}

/// An invalid recording duration bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("recording limit must be between 1 and {MAX_RECORDING_SECONDS} seconds, got {0}")]
pub struct RecordingDurationError(u32);

/// An invalid requested region recording frame rate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("recording frame rate must be between 1 and 60 fps, got {0}")]
pub struct RecordingFpsError(pub u32);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifetime_accepts_bounds() {
        assert_eq!(validate_lifetime(1), Ok(1));
        assert_eq!(
            validate_lifetime(MAX_LIFETIME_SECONDS),
            Ok(MAX_LIFETIME_SECONDS)
        );
    }

    #[test]
    fn lifetime_rejects_values_outside_bounds() {
        assert_eq!(validate_lifetime(0), Err(LifetimeError(0)));
        assert_eq!(
            validate_lifetime(MAX_LIFETIME_SECONDS + 1),
            Err(LifetimeError(MAX_LIFETIME_SECONDS + 1))
        );
    }

    #[test]
    fn recording_frame_rate_is_bounded() {
        assert_eq!(validate_recording_fps(30), Ok(30));
        assert_eq!(validate_recording_fps(0), Err(RecordingFpsError(0)));
        assert_eq!(validate_recording_fps(61), Err(RecordingFpsError(61)));
    }

    #[test]
    fn recording_duration_is_bounded() {
        assert_eq!(validate_recording_seconds(300), Ok(300));
        assert_eq!(
            validate_recording_seconds(0),
            Err(RecordingDurationError(0))
        );
        assert_eq!(
            validate_recording_seconds(3_601),
            Err(RecordingDurationError(3_601))
        );
    }
}
