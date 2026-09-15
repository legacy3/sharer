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

pub(super) fn screenshot_stem(config: &CaptureNamingConfig) -> Result<String> {
    match config.mode {
        CaptureNameMode::ActiveWindow => {
            let title = sharer::capture::active_window_title()
                .filter(|title| !title.eq_ignore_ascii_case("ShareR"));

            Ok(sanitize_stem(title.as_deref().unwrap_or("screenshot")))
        }

        CaptureNameMode::Random => random_stem(&SystemEntropy),

        CaptureNameMode::Friendly => friendly_stem(&SystemEntropy),

        CaptureNameMode::Custom => Ok(sanitize_stem(&config.custom_name)),
    }
}

fn random_stem(entropy: &impl Entropy) -> Result<String> {
    let mut bytes = [0_u8; 16];

    entropy.fill(&mut bytes)?;
    Ok(hex::encode(bytes))
}

fn friendly_stem(entropy: &impl Entropy) -> Result<String> {
    const ADJECTIVES: [&str; 32] = [
        "amber", "brave", "bright", "calm", "clever", "cosmic", "crisp", "daring", "dusky",
        "eager", "fancy", "gentle", "golden", "happy", "hidden", "jolly", "kind", "lively",
        "lucky", "mellow", "mighty", "nimble", "quiet", "rapid", "royal", "silver", "solar",
        "swift", "tidy", "vivid", "warm", "wild",
    ];
    const ANIMALS: [&str; 32] = [
        "badger", "bear", "beaver", "bison", "cat", "crane", "deer", "dolphin", "eagle", "falcon",
        "fox", "gecko", "hare", "heron", "ibis", "koala", "lynx", "marten", "moose", "otter",
        "owl", "panda", "puma", "raven", "seal", "shark", "tiger", "toucan", "turtle", "whale",
        "wolf", "yak",
    ];
    let mut bytes = [0_u8; 4];

    entropy.fill(&mut bytes)?;
    let adjective = ADJECTIVES[usize::from(bytes[0]) % ADJECTIVES.len()];
    let animal = ANIMALS[usize::from(bytes[1]) % ANIMALS.len()];
    let number = u16::from_le_bytes([bytes[2], bytes[3]]) % 10_000;

    Ok(format!("{adjective}-{animal}-{number:04}"))
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
            let pattern = [0x01, 0x23, 0x45, 0x67, 0x89];

            for (index, byte) in bytes.iter_mut().enumerate() {
                *byte = pattern[index % pattern.len()];
            }

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
    fn random_names_use_a_full_128_bits_of_entropy() {
        assert_eq!(
            random_stem(&FixedEntropy).unwrap(),
            "01234567890123456789012345678901"
        );
    }

    #[test]
    fn friendly_names_are_readable_and_include_a_numeric_suffix() {
        assert_eq!(friendly_stem(&FixedEntropy).unwrap(), "brave-bison-6437");
    }
}
