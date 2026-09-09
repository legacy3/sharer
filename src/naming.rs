//! Cross-platform screenshot naming strategies.

use anyhow::Result;
use sharer::config::{CaptureNameMode, CaptureNamingConfig};

trait Entropy {
    fn fill(&self, bytes: &mut [u8]) -> Result<()>;
}

#[derive(Clone, Copy, Debug)]
struct SystemEntropy;

impl Entropy for SystemEntropy {
    fn fill(&self, bytes: &mut [u8]) -> Result<()> {
        getrandom::fill(bytes).map_err(|error| {
            anyhow::anyhow!("failed to generate a private screenshot name: {error}")
        })
    }
}

pub(super) fn screenshot_stem(
    config: &CaptureNamingConfig,
    private_capture_names: bool,
) -> Result<String> {
    match effective_name_mode(config.mode, private_capture_names) {
        CaptureNameMode::ActiveWindow => {
            let title = sharer::capture::active_window_title()
                .filter(|title| !title.eq_ignore_ascii_case("ShareR"));

            Ok(sanitize_stem(title.as_deref().unwrap_or("screenshot")))
        }

        CaptureNameMode::Random => random_stem(&SystemEntropy),

        CaptureNameMode::Custom => Ok(sanitize_stem(&config.custom_name)),
    }
}

const fn effective_name_mode(
    configured: CaptureNameMode,
    private_capture_names: bool,
) -> CaptureNameMode {
    if private_capture_names {
        CaptureNameMode::Random
    } else {
        configured
    }
}

fn random_stem(entropy: &impl Entropy) -> Result<String> {
    let mut bytes = [0_u8; 5];

    entropy.fill(&mut bytes)?;
    Ok(hex::encode(bytes))
}

fn sanitize_stem(value: &str) -> String {
    const MAX_STEM_BYTES: usize = 80;

    let without_extension = value
        .strip_suffix(".png")
        .or_else(|| value.strip_suffix(".PNG"))
        .unwrap_or(value);
    let mut sanitized = String::with_capacity(without_extension.len().min(MAX_STEM_BYTES));
    let mut previous_was_separator = false;

    for character in without_extension.chars() {
        let forbidden = character.is_control()
            || matches!(
                character,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
            );

        let replacement = forbidden || character.is_whitespace();
        let required_bytes = if replacement { 1 } else { character.len_utf8() };

        if sanitized.len().saturating_add(required_bytes) > MAX_STEM_BYTES {
            break;
        }

        if replacement {
            if !previous_was_separator && !sanitized.is_empty() {
                sanitized.push('-');
            }

            previous_was_separator = true;
        } else {
            sanitized.push(character);
            previous_was_separator = false;
        }
    }

    let sanitized = sanitized.trim_matches([' ', '.', '-']);

    if sanitized.is_empty() || is_windows_reserved_stem(sanitized) {
        "screenshot".to_owned()
    } else {
        sanitized.to_owned()
    }
}

pub(super) fn window_title_stem(title: &str) -> String {
    sanitize_stem(title)
}

fn is_windows_reserved_stem(stem: &str) -> bool {
    let stem = stem.split('.').next().unwrap_or(stem);
    let stem = stem.to_ascii_uppercase();

    matches!(
        stem.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct FixedEntropy;

    impl Entropy for FixedEntropy {
        fn fill(&self, bytes: &mut [u8]) -> Result<()> {
            bytes.copy_from_slice(&[0x01, 0x23, 0x45, 0x67, 0x89]);
            Ok(())
        }
    }

    #[test]
    fn names_are_safe_on_all_supported_platforms() {
        assert_eq!(sanitize_stem("Report: Q3 / final?.PNG"), "Report-Q3-final");
        assert_eq!(sanitize_stem("..."), "screenshot");
        assert_eq!(sanitize_stem("CON"), "screenshot");
        assert_eq!(sanitize_stem("lpt9.txt"), "screenshot");
        assert!(sanitize_stem(&"\u{1f980}".repeat(80)).len() <= 80);
    }

    #[test]
    fn random_names_are_short_and_stable_for_injected_entropy() {
        assert_eq!(random_stem(&FixedEntropy).unwrap(), "0123456789");
    }

    #[test]
    fn private_names_override_window_and_static_names() {
        assert_eq!(
            effective_name_mode(CaptureNameMode::ActiveWindow, true),
            CaptureNameMode::Random
        );
        assert_eq!(
            effective_name_mode(CaptureNameMode::Custom, true),
            CaptureNameMode::Random
        );
        assert_eq!(
            effective_name_mode(CaptureNameMode::Custom, false),
            CaptureNameMode::Custom
        );
    }
}
