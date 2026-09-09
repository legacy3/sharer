//! Built-in and custom upload provider implementations.

use std::str::FromStr;

use anyhow::{Result, bail};
use reqwest::{Url, header::HeaderMap, multipart};
use serde::{Deserialize, Serialize};

use super::{UploadReceipt, UploadTarget};

mod custom;
mod imgur;
mod sul;
mod transfer_sh;
mod uguu;
mod vgy;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CredentialRequirement {
    None,
    Optional,
    Required,
}

pub(super) struct ResponseMetadata<'a> {
    pub filename: String,
    pub size_bytes: u64,
    pub delete_header: &'a str,
}

/// Behavior required from an upload provider.
pub(super) trait Provider {
    const NAME: &'static str;
    const FIELD: &'static str;
    const CREDENTIAL: CredentialRequirement = CredentialRequirement::None;
    const IMAGES_ONLY: bool = false;
    const SUPPORTS_LIFETIME: bool = false;

    fn endpoint(target: &UploadTarget, lifetime_seconds: u32) -> Result<Url>;

    fn headers(_target: &UploadTarget) -> Result<HeaderMap> {
        Ok(HeaderMap::new())
    }

    fn form(part: multipart::Part, _target: &UploadTarget) -> multipart::Form {
        multipart::Form::new().part(Self::FIELD, part)
    }

    fn parse(
        body: &[u8],
        target: &UploadTarget,
        metadata: ResponseMetadata<'_>,
    ) -> Result<UploadReceipt>;
}

/// Upload service selected for desktop and headless transfers.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UploaderKind {
    #[default]
    Custom,
    Imgur,
    Vgy,
    Uguu,
    TransferSh,
    Sul,
}

impl UploaderKind {
    /// Convert the settings selector index into an uploader kind.
    #[must_use]
    pub const fn from_index(index: i32) -> Option<Self> {
        match index {
            0 => Some(Self::Custom),
            1 => Some(Self::Imgur),
            2 => Some(Self::Vgy),
            3 => Some(Self::Uguu),
            4 => Some(Self::TransferSh),
            5 => Some(Self::Sul),
            _ => None,
        }
    }

    /// Convert this uploader into its settings selector index.
    #[must_use]
    pub const fn index(self) -> i32 {
        match self {
            Self::Custom => 0,
            Self::Imgur => 1,
            Self::Vgy => 2,
            Self::Uguu => 3,
            Self::TransferSh => 4,
            Self::Sul => 5,
        }
    }

    /// Return the provider name shown to users.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Custom => custom::Custom::NAME,
            Self::Imgur => imgur::Imgur::NAME,
            Self::Vgy => vgy::Vgy::NAME,
            Self::Uguu => uguu::Uguu::NAME,
            Self::TransferSh => transfer_sh::TransferSh::NAME,
            Self::Sul => sul::Sul::NAME,
        }
    }

    /// Return whether the provider accepts a credential value.
    #[must_use]
    pub const fn accepts_credential(self) -> bool {
        !matches!(self.credential_requirement(), CredentialRequirement::None)
    }

    /// Return whether the provider requires a credential value.
    #[must_use]
    pub const fn requires_credential(self) -> bool {
        matches!(
            self.credential_requirement(),
            CredentialRequirement::Required
        )
    }

    /// Return whether the provider implements `ShareR`'s lifetime query.
    #[must_use]
    pub const fn supports_lifetime(self) -> bool {
        match self {
            Self::Custom => custom::Custom::SUPPORTS_LIFETIME,
            Self::Imgur => imgur::Imgur::SUPPORTS_LIFETIME,
            Self::Vgy => vgy::Vgy::SUPPORTS_LIFETIME,
            Self::Uguu => uguu::Uguu::SUPPORTS_LIFETIME,
            Self::TransferSh => transfer_sh::TransferSh::SUPPORTS_LIFETIME,
            Self::Sul => sul::Sul::SUPPORTS_LIFETIME,
        }
    }

    pub(super) const fn images_only(self) -> bool {
        match self {
            Self::Custom => custom::Custom::IMAGES_ONLY,
            Self::Imgur => imgur::Imgur::IMAGES_ONLY,
            Self::Vgy => vgy::Vgy::IMAGES_ONLY,
            Self::Uguu => uguu::Uguu::IMAGES_ONLY,
            Self::TransferSh => transfer_sh::TransferSh::IMAGES_ONLY,
            Self::Sul => sul::Sul::IMAGES_ONLY,
        }
    }

    pub(super) fn endpoint(self, target: &UploadTarget, lifetime_seconds: u32) -> Result<Url> {
        match self {
            Self::Custom => custom::Custom::endpoint(target, lifetime_seconds),
            Self::Imgur => imgur::Imgur::endpoint(target, lifetime_seconds),
            Self::Vgy => vgy::Vgy::endpoint(target, lifetime_seconds),
            Self::Uguu => uguu::Uguu::endpoint(target, lifetime_seconds),
            Self::TransferSh => transfer_sh::TransferSh::endpoint(target, lifetime_seconds),
            Self::Sul => sul::Sul::endpoint(target, lifetime_seconds),
        }
    }

    pub(super) fn headers(self, target: &UploadTarget) -> Result<HeaderMap> {
        match self {
            Self::Custom => custom::Custom::headers(target),
            Self::Imgur => imgur::Imgur::headers(target),
            Self::Vgy => vgy::Vgy::headers(target),
            Self::Uguu => uguu::Uguu::headers(target),
            Self::TransferSh => transfer_sh::TransferSh::headers(target),
            Self::Sul => sul::Sul::headers(target),
        }
    }

    pub(super) fn form(self, part: multipart::Part, target: &UploadTarget) -> multipart::Form {
        match self {
            Self::Custom => custom::Custom::form(part, target),
            Self::Imgur => imgur::Imgur::form(part, target),
            Self::Vgy => vgy::Vgy::form(part, target),
            Self::Uguu => uguu::Uguu::form(part, target),
            Self::TransferSh => transfer_sh::TransferSh::form(part, target),
            Self::Sul => sul::Sul::form(part, target),
        }
    }

    pub(super) fn parse(
        self,
        body: &[u8],
        target: &UploadTarget,
        metadata: ResponseMetadata<'_>,
    ) -> Result<UploadReceipt> {
        match self {
            Self::Custom => custom::Custom::parse(body, target, metadata),
            Self::Imgur => imgur::Imgur::parse(body, target, metadata),
            Self::Vgy => vgy::Vgy::parse(body, target, metadata),
            Self::Uguu => uguu::Uguu::parse(body, target, metadata),
            Self::TransferSh => transfer_sh::TransferSh::parse(body, target, metadata),
            Self::Sul => sul::Sul::parse(body, target, metadata),
        }
    }

    const fn credential_requirement(self) -> CredentialRequirement {
        match self {
            Self::Custom => custom::Custom::CREDENTIAL,
            Self::Imgur => imgur::Imgur::CREDENTIAL,
            Self::Vgy => vgy::Vgy::CREDENTIAL,
            Self::Uguu => uguu::Uguu::CREDENTIAL,
            Self::TransferSh => transfer_sh::TransferSh::CREDENTIAL,
            Self::Sul => sul::Sul::CREDENTIAL,
        }
    }
}

impl FromStr for UploaderKind {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let normalized = value.trim().to_ascii_lowercase();

        match normalized.as_str() {
            "custom" => Ok(Self::Custom),
            "imgur" => Ok(Self::Imgur),
            "vgy" | "vgy.me" => Ok(Self::Vgy),
            "uguu" => Ok(Self::Uguu),
            "transfer" | "transfer.sh" | "transfer-sh" => Ok(Self::TransferSh),
            "s-ul" | "sul" | "s-ul.eu" => Ok(Self::Sul),
            _ => bail!("unknown uploader '{value}'"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_names_accept_common_spellings() {
        assert_eq!("vgy.me".parse::<UploaderKind>().unwrap(), UploaderKind::Vgy);
        assert_eq!(
            "transfer-sh".parse::<UploaderKind>().unwrap(),
            UploaderKind::TransferSh
        );
        assert_eq!("sul".parse::<UploaderKind>().unwrap(), UploaderKind::Sul);
    }
}
