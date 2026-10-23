//! Provider-specific retention choices kept independent of custom-server expiry.

use serde::{Deserialize, Serialize};

/// Retention periods supported by the Litterbox upload API.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum LitterboxRetention {
    #[serde(rename = "1h")]
    OneHour,
    #[serde(rename = "12h")]
    TwelveHours,
    #[serde(rename = "24h")]
    TwentyFourHours,
    #[default]
    #[serde(rename = "72h")]
    SeventyTwoHours,
}

impl LitterboxRetention {
    /// Parse a desktop selector index without accepting unsupported expiry values.
    #[must_use]
    pub const fn from_index(index: i32) -> Option<Self> {
        match index {
            0 => Some(Self::OneHour),
            1 => Some(Self::TwelveHours),
            2 => Some(Self::TwentyFourHours),
            3 => Some(Self::SeventyTwoHours),
            _ => None,
        }
    }

    /// Return the desktop selector index.
    #[must_use]
    pub const fn index(self) -> i32 {
        match self {
            Self::OneHour => 0,
            Self::TwelveHours => 1,
            Self::TwentyFourHours => 2,
            Self::SeventyTwoHours => 3,
        }
    }

    /// Return the multipart value accepted by Litterbox.
    #[must_use]
    pub const fn api_value(self) -> &'static str {
        match self {
            Self::OneHour => "1h",
            Self::TwelveHours => "12h",
            Self::TwentyFourHours => "24h",
            Self::SeventyTwoHours => "72h",
        }
    }

    /// Describe the requested duration without inventing a server-confirmed timestamp.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::OneHour => "1 hour (requested)",
            Self::TwelveHours => "12 hours (requested)",
            Self::TwentyFourHours => "24 hours (requested)",
            Self::SeventyTwoHours => "72 hours (requested)",
        }
    }
}
