use anyhow::{Context, Result};
use reqwest::{Url, multipart};

use super::{Provider, ResponseMetadata};
use crate::upload::{
    UploadReceipt, UploadTarget, parse_uploader_url, plain_receipt, validate_response_url,
};

const UPLOAD_URL: &str = "https://litterbox.catbox.moe/resources/internals/api.php";

#[derive(Debug)]
pub(super) struct Litterbox;

impl Provider for Litterbox {
    const NAME: &'static str = "Litterbox";
    const FIELD: &'static str = "fileToUpload";

    fn endpoint(_target: &UploadTarget, _lifetime_seconds: u32) -> Result<Url> {
        parse_uploader_url(UPLOAD_URL)
    }

    fn form(part: multipart::Part, target: &UploadTarget) -> multipart::Form {
        multipart::Form::new()
            .text("reqtype", "fileupload")
            .text("time", target.litterbox_retention.api_value())
            .part(Self::FIELD, part)
    }

    fn parse(
        body: &[u8],
        target: &UploadTarget,
        metadata: ResponseMetadata<'_>,
    ) -> Result<UploadReceipt> {
        let mut receipt =
            plain_receipt(body, metadata.filename, metadata.size_bytes, String::new())?;

        validate_response_url(&receipt.link, "Litterbox public link")
            .context("Litterbox did not return an upload link")?;
        target
            .litterbox_retention
            .description()
            .clone_into(&mut receipt.expires_at);
        Ok(receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upload::{LitterboxRetention, providers::tests::form_body};

    #[test]
    fn request_uses_each_supported_retention_without_credentials_or_custom_expiry() {
        for (retention, time) in [
            (LitterboxRetention::OneHour, "1h"),
            (LitterboxRetention::TwelveHours, "12h"),
            (LitterboxRetention::TwentyFourHours, "24h"),
            (LitterboxRetention::SeventyTwoHours, "72h"),
        ] {
            let target = UploadTarget {
                litterbox_retention: retention,
                credential: "must-not-leak".to_owned(),
                header_name: "X-Private".to_owned(),
                header_value: "must-not-leak".to_owned(),
                ..UploadTarget::default()
            };
            let body = form_body(Litterbox::form(multipart::Part::text("capture"), &target));

            for custom_lifetime in [0, 90, 604_800] {
                assert_eq!(
                    Litterbox::endpoint(&target, custom_lifetime)
                        .unwrap()
                        .as_str(),
                    UPLOAD_URL
                );
            }

            assert!(Litterbox::headers(&target).unwrap().is_empty());
            assert!(body.contains("name=\"reqtype\"\r\n\r\nfileupload\r\n"));
            assert!(body.contains(&format!("name=\"time\"\r\n\r\n{time}\r\n")));
            assert!(body.contains("name=\"fileToUpload\"\r\n\r\ncapture\r\n"));
            assert!(!body.contains("must-not-leak"));
        }
    }

    #[test]
    fn plaintext_receipt_labels_requested_retention_without_an_invented_timestamp() {
        let target = UploadTarget {
            litterbox_retention: LitterboxRetention::TwelveHours,
            ..UploadTarget::default()
        };
        let receipt = Litterbox::parse(
            b"https://litter.catbox.moe/example.png\n",
            &target,
            ResponseMetadata {
                filename: "capture.png".to_owned(),
                size_bytes: 42,
                delete_header: "",
            },
        )
        .unwrap();

        assert_eq!(receipt.link, "https://litter.catbox.moe/example.png");
        assert_eq!(receipt.expires_at, "12 hours (requested)");
        assert!(receipt.delete_url.is_empty());
    }

    #[test]
    fn rejects_plaintext_failure_response() {
        assert!(
            Litterbox::parse(
                b"File is too large",
                &UploadTarget::default(),
                ResponseMetadata {
                    filename: "capture.png".to_owned(),
                    size_bytes: 42,
                    delete_header: "",
                },
            )
            .is_err()
        );
    }
}
