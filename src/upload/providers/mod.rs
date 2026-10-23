//! Built-in and custom upload provider implementations.

use std::str::FromStr;

use anyhow::{Result, bail};
use reqwest::{Url, header::HeaderMap, multipart};
use serde::{Deserialize, Serialize};

use super::{UploadReceipt, UploadTarget};

mod catbox;
mod custom;
mod imgur;
mod litterbox;
mod pixeldrain;
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
    Catbox,
    Litterbox,
    Pixeldrain,
    Imgur,
    Vgy,
    Uguu,
    TransferSh,
    Sul,
}

impl UploaderKind {
    /// Providers offered for new configurations, in the desktop selector order.
    pub const AVAILABLE: [Self; 7] = [
        Self::Custom,
        Self::Uguu,
        Self::Catbox,
        Self::Litterbox,
        Self::Imgur,
        Self::Pixeldrain,
        Self::Sul,
    ];

    /// Convert the settings selector index into an uploader kind.
    #[must_use]
    pub const fn from_index(index: i32) -> Option<Self> {
        match index {
            0 => Some(Self::Custom),
            1 => Some(Self::Uguu),
            2 => Some(Self::Catbox),
            3 => Some(Self::Litterbox),
            4 => Some(Self::Imgur),
            5 => Some(Self::Pixeldrain),
            6 => Some(Self::Sul),
            7 => Some(Self::Vgy),
            8 => Some(Self::TransferSh),
            _ => None,
        }
    }

    /// Return whether the provider is available for new uploads.
    #[must_use]
    pub const fn is_available(self) -> bool {
        self.retirement_message().is_none()
    }

    /// Explain why a saved legacy destination needs a new explicit selection.
    #[must_use]
    pub const fn retirement_message(self) -> Option<&'static str> {
        match self {
            Self::Vgy => Some(
                "vgy.me is no longer offered because its upload API could not be verified. Choose another provider in Settings.",
            ),

            Self::TransferSh => Some(
                "The public transfer.sh service is unavailable. Choose another provider in Settings, or configure your own server.",
            ),

            _ => None,
        }
    }

    /// Public service limits shown wherever users choose an upload destination.
    #[must_use]
    pub const fn limits_description(self) -> &'static str {
        match self {
            Self::Custom => "Your server sets size and retention limits.",

            Self::Uguu => "128 MiB per file · 3 hours",

            Self::Catbox => "200 MB; GIF 20 MB · 2 years without access",

            Self::Litterbox => "1 GB per file · 1, 12, 24 or 72 hours",

            Self::Imgur => {
                "Client ID required · JPEG, PNG, GIF, APNG, TIFF · May compress images; unavailable in the UK"
            }

            Self::Pixeldrain => "API key required · Free: 10 GB per file, 60 days without access",

            Self::Sul => "Account key required · Limits not publicly documented",

            Self::Vgy => {
                "No longer offered: upload API could not be verified. Choose another provider."
            }

            Self::TransferSh => {
                "Public service unavailable. Choose another provider or your own server."
            }
        }
    }

    /// Convert this uploader into its settings selector index.
    #[must_use]
    pub const fn index(self) -> i32 {
        match self {
            Self::Custom => 0,
            Self::Uguu => 1,
            Self::Catbox => 2,
            Self::Litterbox => 3,
            Self::Imgur => 4,
            Self::Pixeldrain => 5,
            Self::Sul => 6,
            Self::Vgy => 7,
            Self::TransferSh => 8,
        }
    }

    /// Return the provider name shown to users.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Custom => custom::Custom::NAME,
            Self::Catbox => catbox::Catbox::NAME,
            Self::Litterbox => litterbox::Litterbox::NAME,
            Self::Pixeldrain => pixeldrain::Pixeldrain::NAME,
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
            Self::Catbox => catbox::Catbox::SUPPORTS_LIFETIME,
            Self::Litterbox => litterbox::Litterbox::SUPPORTS_LIFETIME,
            Self::Pixeldrain => pixeldrain::Pixeldrain::SUPPORTS_LIFETIME,
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
            Self::Catbox => catbox::Catbox::IMAGES_ONLY,
            Self::Litterbox => litterbox::Litterbox::IMAGES_ONLY,
            Self::Pixeldrain => pixeldrain::Pixeldrain::IMAGES_ONLY,
            Self::Imgur => imgur::Imgur::IMAGES_ONLY,
            Self::Vgy => vgy::Vgy::IMAGES_ONLY,
            Self::Uguu => uguu::Uguu::IMAGES_ONLY,
            Self::TransferSh => transfer_sh::TransferSh::IMAGES_ONLY,
            Self::Sul => sul::Sul::IMAGES_ONLY,
        }
    }

    pub(super) fn validate_payload(self, mime: &str, size_bytes: u64) -> Result<()> {
        anyhow::ensure!(
            !self.images_only() || mime.starts_with("image/"),
            "{} only accepts images",
            self.name()
        );

        if self == Self::Imgur {
            anyhow::ensure!(
                matches!(
                    mime,
                    "image/jpeg"
                        | "image/jpg"
                        | "image/png"
                        | "image/gif"
                        | "image/apng"
                        | "image/tiff"
                ),
                "Imgur accepts JPEG, PNG, GIF, APNG and TIFF images; choose another provider for this file format"
            );
        }

        // Uguu publishes its exact limit in MiB. Other services have plan-specific
        // or ambiguous MB limits; their API remains authoritative for those sizes.
        if self == Self::Uguu {
            anyhow::ensure!(
                size_bytes <= 128 * 1024 * 1024,
                "Uguu accepts files up to 128 MiB"
            );
        }

        Ok(())
    }

    pub(super) fn endpoint(self, target: &UploadTarget, lifetime_seconds: u32) -> Result<Url> {
        match self {
            Self::Custom => custom::Custom::endpoint(target, lifetime_seconds),
            Self::Catbox => catbox::Catbox::endpoint(target, lifetime_seconds),
            Self::Litterbox => litterbox::Litterbox::endpoint(target, lifetime_seconds),
            Self::Pixeldrain => pixeldrain::Pixeldrain::endpoint(target, lifetime_seconds),
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
            Self::Catbox => catbox::Catbox::headers(target),
            Self::Litterbox => litterbox::Litterbox::headers(target),
            Self::Pixeldrain => pixeldrain::Pixeldrain::headers(target),
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
            Self::Catbox => catbox::Catbox::form(part, target),
            Self::Litterbox => litterbox::Litterbox::form(part, target),
            Self::Pixeldrain => pixeldrain::Pixeldrain::form(part, target),
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
            Self::Catbox => catbox::Catbox::parse(body, target, metadata),
            Self::Litterbox => litterbox::Litterbox::parse(body, target, metadata),
            Self::Pixeldrain => pixeldrain::Pixeldrain::parse(body, target, metadata),
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
            Self::Catbox => catbox::Catbox::CREDENTIAL,
            Self::Litterbox => litterbox::Litterbox::CREDENTIAL,
            Self::Pixeldrain => pixeldrain::Pixeldrain::CREDENTIAL,
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
            "catbox" | "catbox.moe" => Ok(Self::Catbox),
            "litterbox" | "litterbox.catbox.moe" => Ok(Self::Litterbox),
            "pixeldrain" | "pixeldrain.com" => Ok(Self::Pixeldrain),
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

    pub(super) fn form_body(form: multipart::Form) -> String {
        use futures_util::StreamExt;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let body = runtime.block_on(async {
            let stream = form.into_stream();

            futures_util::pin_mut!(stream);
            let mut body = Vec::new();

            while let Some(chunk) = stream.next().await {
                body.extend_from_slice(&chunk.unwrap());
            }

            body
        });

        String::from_utf8(body).unwrap()
    }

    #[test]
    fn active_catalog_indices_are_contiguous_and_legacy_ids_stay_distinct() {
        for (index, kind) in UploaderKind::AVAILABLE.into_iter().enumerate() {
            let index = i32::try_from(index).unwrap();

            assert_eq!(kind.index(), index);
            assert_eq!(UploaderKind::from_index(index), Some(kind));
            assert!(kind.is_available());
            assert!(!kind.limits_description().is_empty());
        }

        for kind in [UploaderKind::Vgy, UploaderKind::TransferSh] {
            assert!(!kind.is_available());
            assert!(!UploaderKind::AVAILABLE.contains(&kind));
            assert_eq!(UploaderKind::from_index(kind.index()), Some(kind));
        }

        assert_eq!(UploaderKind::from_index(-1), None);
        assert_eq!(UploaderKind::from_index(9), None);
    }

    #[test]
    fn saved_provider_names_round_trip_without_migration() {
        for kind in UploaderKind::AVAILABLE
            .into_iter()
            .chain([UploaderKind::Vgy, UploaderKind::TransferSh])
        {
            let encoded = serde_json::to_string(&kind).unwrap();

            assert_eq!(
                serde_json::from_str::<UploaderKind>(&encoded).unwrap(),
                kind
            );
        }

        assert_eq!(
            serde_json::from_str::<UploaderKind>("\"vgy\"").unwrap(),
            UploaderKind::Vgy
        );
        assert_eq!(
            serde_json::from_str::<UploaderKind>("\"transfer_sh\"").unwrap(),
            UploaderKind::TransferSh
        );
    }

    #[test]
    fn validates_documented_file_size_and_image_only_restrictions() {
        assert!(
            UploaderKind::Uguu
                .validate_payload("image/png", 128 * 1024 * 1024)
                .is_ok()
        );
        assert!(
            UploaderKind::Uguu
                .validate_payload("image/png", 128 * 1024 * 1024 + 1)
                .is_err()
        );
        assert!(
            UploaderKind::Imgur
                .validate_payload("video/mp4", 1)
                .is_err()
        );
        assert!(UploaderKind::Imgur.validate_payload("image/png", 1).is_ok());
    }

    #[test]
    fn imgur_rejects_undocumented_formats_before_upload() {
        for mime in [
            "image/jpeg",
            "image/jpg",
            "image/png",
            "image/gif",
            "image/apng",
            "image/tiff",
        ] {
            assert!(
                UploaderKind::Imgur.validate_payload(mime, 42).is_ok(),
                "{mime}"
            );
        }

        for mime in [
            "image/webp",
            "image/svg+xml",
            "image/avif",
            "image/bmp",
            "video/mp4",
            "application/octet-stream",
        ] {
            assert!(
                UploaderKind::Imgur.validate_payload(mime, 42).is_err(),
                "{mime}"
            );
        }
    }

    #[test]
    fn cli_names_accept_common_spellings() {
        assert_eq!("vgy.me".parse::<UploaderKind>().unwrap(), UploaderKind::Vgy);
        assert_eq!(
            "transfer-sh".parse::<UploaderKind>().unwrap(),
            UploaderKind::TransferSh
        );
        assert_eq!("sul".parse::<UploaderKind>().unwrap(), UploaderKind::Sul);
        assert_eq!(
            "catbox.moe".parse::<UploaderKind>().unwrap(),
            UploaderKind::Catbox
        );
        assert_eq!(
            "litterbox".parse::<UploaderKind>().unwrap(),
            UploaderKind::Litterbox
        );
        assert_eq!(
            "pixeldrain.com".parse::<UploaderKind>().unwrap(),
            UploaderKind::Pixeldrain
        );
    }
}
