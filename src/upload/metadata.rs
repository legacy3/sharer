//! Streaming and in-place EXIF removal for supported image containers.

use std::io::{Read, SeekFrom};

use anyhow::{Context, Result, bail};

use super::ReadSeek;

#[derive(Clone, Copy)]
pub(super) enum MetadataFormat {
    Jpeg,
    Png,
    Webp,
}

pub(super) fn load_and_strip_exif(
    reader: &mut dyn ReadSeek,
    length: u64,
    format: MetadataFormat,
) -> Result<Option<Vec<u8>>> {
    let contains_exif = reader_contains_exif(reader, length, format)?;

    reader
        .seek(SeekFrom::Start(0))
        .context("failed to rewind image after EXIF inspection")?;

    if !contains_exif {
        return Ok(None);
    }

    let capacity = usize::try_from(length).context("image is too large to inspect for EXIF")?;
    let mut bytes = Vec::with_capacity(capacity);

    reader
        .take(length)
        .read_to_end(&mut bytes)
        .context("failed to read image for EXIF removal")?;
    anyhow::ensure!(bytes.len() == capacity, "image ended during EXIF removal");
    strip_exif_in_place(&mut bytes, format)?;
    reader
        .seek(SeekFrom::Start(0))
        .context("failed to rewind image after EXIF removal")?;

    Ok(Some(bytes))
}

pub(super) fn strip_exif_in_place(bytes: &mut Vec<u8>, format: MetadataFormat) -> Result<bool> {
    match format {
        MetadataFormat::Jpeg => strip_jpeg_exif(bytes),
        MetadataFormat::Png => strip_png_exif(bytes),
        MetadataFormat::Webp => strip_webp_exif(bytes),
    }
}

fn reader_contains_exif(
    reader: &mut dyn ReadSeek,
    expected_length: u64,
    format: MetadataFormat,
) -> Result<bool> {
    let actual_length = reader
        .seek(SeekFrom::End(0))
        .context("failed to inspect image length")?;

    anyhow::ensure!(
        actual_length == expected_length,
        "image length changed during inspection"
    );
    reader
        .seek(SeekFrom::Start(0))
        .context("failed to rewind image before EXIF inspection")?;

    match format {
        MetadataFormat::Jpeg => reader_contains_jpeg_exif(reader, actual_length),
        MetadataFormat::Png => reader_contains_png_exif(reader, actual_length),
        MetadataFormat::Webp => reader_contains_webp_exif(reader, actual_length),
    }
}

fn read_exact<const SIZE: usize>(
    reader: &mut dyn ReadSeek,
    description: &str,
) -> Result<[u8; SIZE]> {
    let mut bytes = [0_u8; SIZE];

    reader
        .read_exact(&mut bytes)
        .with_context(|| description.to_owned())?;
    Ok(bytes)
}

fn seek_checked(
    reader: &mut dyn ReadSeek,
    position: u64,
    length: u64,
    message: &str,
) -> Result<()> {
    anyhow::ensure!(position <= length, "{message}");
    reader
        .seek(SeekFrom::Start(position))
        .with_context(|| message.to_owned())?;
    Ok(())
}

fn reader_contains_jpeg_exif(reader: &mut dyn ReadSeek, length: u64) -> Result<bool> {
    let header = read_exact::<2>(reader, "failed to read JPEG header")?;

    anyhow::ensure!(header == [0xff, 0xd8], "JPEG upload has an invalid header");
    let mut position = 2_u64;
    let mut found = false;

    while position < length {
        let mut marker = read_exact::<1>(reader, "JPEG upload ends inside a marker")?[0];

        position += 1;

        if marker != 0xff {
            break;
        }

        while marker == 0xff {
            marker = read_exact::<1>(reader, "JPEG upload ends inside a marker")?[0];
            position += 1;
        }

        if marker == 0xda || marker == 0xd9 {
            break;
        }

        if marker == 0x01 || (0xd0..=0xd8).contains(&marker) {
            continue;
        }

        let segment_length = u64::from(u16::from_be_bytes(read_exact::<2>(
            reader,
            "JPEG upload has a truncated segment",
        )?));

        anyhow::ensure!(
            segment_length >= 2,
            "JPEG upload has an invalid segment length"
        );
        let segment_end = position
            .checked_add(segment_length)
            .context("JPEG segment length overflow")?;

        seek_checked(
            reader,
            segment_end,
            length,
            "JPEG upload has a truncated segment",
        )?;

        if marker == 0xe1 && segment_length >= 8 {
            reader
                .seek(SeekFrom::Start(position + 2))
                .context("failed to inspect JPEG EXIF marker")?;
            found |=
                read_exact::<6>(reader, "JPEG upload has a truncated segment")? == *b"Exif\0\0";
            reader
                .seek(SeekFrom::Start(segment_end))
                .context("failed to continue JPEG inspection")?;
        }

        position = segment_end;
    }

    Ok(found)
}

fn reader_contains_png_exif(reader: &mut dyn ReadSeek, length: u64) -> Result<bool> {
    const SIGNATURE: [u8; 8] = *b"\x89PNG\r\n\x1a\n";
    let header = read_exact::<8>(reader, "failed to read PNG header")?;

    anyhow::ensure!(header == SIGNATURE, "PNG upload has an invalid header");
    let mut position = 8_u64;
    let mut found = false;

    while position < length {
        let chunk = read_exact::<8>(reader, "PNG upload has a truncated chunk")?;
        let data_length = u64::from(u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        let chunk_end = position
            .checked_add(12)
            .and_then(|end| end.checked_add(data_length))
            .context("PNG chunk length overflow")?;

        found |= &chunk[4..] == b"eXIf";
        seek_checked(
            reader,
            chunk_end,
            length,
            "PNG upload has a truncated chunk",
        )?;
        position = chunk_end;
    }

    Ok(found)
}

fn reader_contains_webp_exif(reader: &mut dyn ReadSeek, length: u64) -> Result<bool> {
    let header = read_exact::<12>(reader, "failed to read WebP header")?;

    anyhow::ensure!(
        &header[..4] == b"RIFF" && &header[8..] == b"WEBP",
        "WebP upload has an invalid header"
    );
    let mut position = 12_u64;
    let mut found = false;

    while position < length {
        let chunk = read_exact::<8>(reader, "WebP upload has a truncated chunk")?;
        let data_length = u64::from(u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]));
        let padded_length = data_length
            .checked_add(data_length % 2)
            .context("WebP chunk overflow")?;
        let chunk_end = position
            .checked_add(8)
            .and_then(|end| end.checked_add(padded_length))
            .context("WebP chunk length overflow")?;

        found |= &chunk[..4] == b"EXIF";
        seek_checked(
            reader,
            chunk_end,
            length,
            "WebP upload has a truncated chunk",
        )?;
        position = chunk_end;
    }

    Ok(found)
}

pub(super) fn strip_jpeg_exif(bytes: &mut Vec<u8>) -> Result<bool> {
    if !jpeg_contains_exif(bytes)? {
        return Ok(false);
    }

    let length = bytes.len();
    let mut read_position = 2;
    let mut write_position = 2;

    while read_position < length {
        let segment_start = read_position;

        if bytes[read_position] != 0xff {
            bytes.copy_within(read_position..length, write_position);
            write_position += length - read_position;
            break;
        }

        while bytes[read_position] == 0xff {
            read_position += 1;
        }

        let marker = bytes[read_position];

        read_position += 1;

        if marker == 0xda || marker == 0xd9 {
            bytes.copy_within(segment_start..length, write_position);
            write_position += length - segment_start;
            break;
        }

        if marker == 0x01 || (0xd0..=0xd8).contains(&marker) {
            bytes.copy_within(segment_start..read_position, write_position);
            write_position += read_position - segment_start;
            continue;
        }

        let segment_length = usize::from(u16::from_be_bytes([
            bytes[read_position],
            bytes[read_position + 1],
        ]));
        let segment_end = read_position + segment_length;
        let data_start = read_position + 2;
        let is_exif = marker == 0xe1 && bytes[data_start..segment_end].starts_with(b"Exif\0\0");

        if !is_exif {
            bytes.copy_within(segment_start..segment_end, write_position);
            write_position += segment_end - segment_start;
        }

        read_position = segment_end;
    }

    bytes.truncate(write_position);
    Ok(true)
}

pub(super) fn jpeg_contains_exif(bytes: &[u8]) -> Result<bool> {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        bail!("JPEG upload has an invalid header");
    }

    let mut position = 2;
    let mut found = false;

    while position < bytes.len() {
        if bytes[position] != 0xff {
            break;
        }

        while position < bytes.len() && bytes[position] == 0xff {
            position += 1;
        }

        anyhow::ensure!(position < bytes.len(), "JPEG upload ends inside a marker");
        let marker = bytes[position];

        position += 1;

        if marker == 0xda || marker == 0xd9 {
            break;
        }

        if marker == 0x01 || (0xd0..=0xd8).contains(&marker) {
            continue;
        }

        anyhow::ensure!(
            position + 2 <= bytes.len(),
            "JPEG upload has a truncated segment"
        );
        let segment_length =
            usize::from(u16::from_be_bytes([bytes[position], bytes[position + 1]]));

        anyhow::ensure!(
            segment_length >= 2,
            "JPEG upload has an invalid segment length"
        );
        let segment_end = position
            .checked_add(segment_length)
            .context("JPEG segment length overflow")?;

        anyhow::ensure!(
            segment_end <= bytes.len(),
            "JPEG upload has a truncated segment"
        );
        let data_start = position + 2;
        let is_exif = marker == 0xe1 && bytes[data_start..segment_end].starts_with(b"Exif\0\0");

        found |= is_exif;

        position = segment_end;
    }

    Ok(found)
}

pub(super) fn strip_png_exif(bytes: &mut Vec<u8>) -> Result<bool> {
    const SIGNATURE_LENGTH: usize = 8;

    if !png_contains_exif(bytes)? {
        return Ok(false);
    }

    let length = bytes.len();
    let mut read_position = SIGNATURE_LENGTH;
    let mut write_position = SIGNATURE_LENGTH;

    while read_position < length {
        let data_length = usize::try_from(u32::from_be_bytes([
            bytes[read_position],
            bytes[read_position + 1],
            bytes[read_position + 2],
            bytes[read_position + 3],
        ]))
        .context("PNG chunk is too large")?;
        let chunk_end = read_position + 12 + data_length;

        if &bytes[read_position + 4..read_position + 8] != b"eXIf" {
            bytes.copy_within(read_position..chunk_end, write_position);
            write_position += chunk_end - read_position;
        }

        read_position = chunk_end;
    }

    bytes.truncate(write_position);
    Ok(true)
}

fn png_contains_exif(bytes: &[u8]) -> Result<bool> {
    const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

    anyhow::ensure!(
        bytes.starts_with(SIGNATURE),
        "PNG upload has an invalid header"
    );
    let mut position = SIGNATURE.len();
    let mut found = false;

    while position < bytes.len() {
        anyhow::ensure!(
            position + 12 <= bytes.len(),
            "PNG upload has a truncated chunk"
        );
        let length = usize::try_from(u32::from_be_bytes([
            bytes[position],
            bytes[position + 1],
            bytes[position + 2],
            bytes[position + 3],
        ]))
        .context("PNG chunk is too large")?;
        let chunk_end = position
            .checked_add(12)
            .and_then(|end| end.checked_add(length))
            .context("PNG chunk length overflow")?;

        anyhow::ensure!(chunk_end <= bytes.len(), "PNG upload has a truncated chunk");

        if &bytes[position + 4..position + 8] == b"eXIf" {
            found = true;
        }

        position = chunk_end;
    }

    Ok(found)
}

pub(super) fn strip_webp_exif(bytes: &mut Vec<u8>) -> Result<bool> {
    if !webp_contains_exif(bytes)? {
        return Ok(false);
    }

    let length = bytes.len();
    let mut read_position = 12;
    let mut write_position = 12;

    while read_position < length {
        let data_length = usize::try_from(u32::from_le_bytes([
            bytes[read_position + 4],
            bytes[read_position + 5],
            bytes[read_position + 6],
            bytes[read_position + 7],
        ]))
        .context("WebP chunk is too large")?;
        let chunk_end = read_position + 8 + data_length + data_length % 2;

        if &bytes[read_position..read_position + 4] != b"EXIF" {
            bytes.copy_within(read_position..chunk_end, write_position);

            if &bytes[write_position..write_position + 4] == b"VP8X" && data_length > 0 {
                const EXIF_FEATURE_FLAG: u8 = 0x08;

                bytes[write_position + 8] &= !EXIF_FEATURE_FLAG;
            }

            write_position += chunk_end - read_position;
        }

        read_position = chunk_end;
    }

    bytes.truncate(write_position);
    let riff_size =
        u32::try_from(bytes.len().saturating_sub(8)).context("WebP upload is too large")?;

    bytes[4..8].copy_from_slice(&riff_size.to_le_bytes());
    Ok(true)
}

fn webp_contains_exif(bytes: &[u8]) -> Result<bool> {
    anyhow::ensure!(
        bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        "WebP upload has an invalid header"
    );
    let mut position = 12;
    let mut found = false;

    while position < bytes.len() {
        anyhow::ensure!(
            position + 8 <= bytes.len(),
            "WebP upload has a truncated chunk"
        );
        let length = usize::try_from(u32::from_le_bytes([
            bytes[position + 4],
            bytes[position + 5],
            bytes[position + 6],
            bytes[position + 7],
        ]))
        .context("WebP chunk is too large")?;
        let padded_length = length
            .checked_add(length % 2)
            .context("WebP chunk overflow")?;
        let chunk_end = position
            .checked_add(8)
            .and_then(|end| end.checked_add(padded_length))
            .context("WebP chunk length overflow")?;

        anyhow::ensure!(
            chunk_end <= bytes.len(),
            "WebP upload has a truncated chunk"
        );

        if &bytes[position..position + 4] == b"EXIF" {
            found = true;
        }

        position = chunk_end;
    }

    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upload::UploadPayload;

    #[test]
    fn exif_removal_preserves_sdr_pixels_and_color_profile() {
        let fixture = include_bytes!("../../tests/fixtures/images/minnie_sdr_with_exif.jpg");
        let before_metadata = metastrip::extract_metadata(fixture).unwrap();
        let before_pixels = image::load_from_memory(fixture).unwrap().to_rgba8();
        let mut payload = UploadPayload::from_bytes(
            fixture.to_vec(),
            "minnie.jpg".to_owned(),
            "image/jpeg".to_owned(),
        );

        assert!(before_metadata.exif.is_some());
        assert!(before_metadata.icc_profile.is_some());
        payload.remove_exif().unwrap();

        let stripped = payload.bytes().unwrap();
        let after_metadata = metastrip::extract_metadata(stripped).unwrap();
        let after_pixels = image::load_from_memory(stripped).unwrap().to_rgba8();

        assert!(after_metadata.exif.is_none());
        assert!(after_metadata.icc_profile.is_some());
        assert_eq!(after_pixels, before_pixels);
    }

    #[test]
    fn file_backed_exif_is_detected_before_buffering() {
        let fixture = include_bytes!("../../tests/fixtures/images/minnie_sdr_with_exif.jpg");
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fixture.jpg");

        std::fs::write(&path, fixture).unwrap();
        let mut payload = UploadPayload::from_path(&path).unwrap();

        assert!(payload.bytes().is_none());
        payload.remove_exif().unwrap();

        let stripped = payload.bytes().unwrap();

        assert!(jpeg_contains_exif(fixture).unwrap());
        assert!(!jpeg_contains_exif(stripped).unwrap());
    }

    #[test]
    fn exif_removal_preserves_hdr_gain_map_structure() {
        let fixture = include_bytes!("../../tests/fixtures/images/apple_gainmap_new.jpg");
        let before_pixels = image::load_from_memory(fixture).unwrap().to_rgba8();
        let mut payload = UploadPayload::from_bytes(
            fixture.to_vec(),
            "gainmap.jpg".to_owned(),
            "image/jpeg".to_owned(),
        );

        assert!(jpeg_contains_exif(fixture).unwrap());
        assert!(fixture.windows(4).any(|window| window == b"MPF\0"));
        assert!(fixture.windows(12).any(|window| window == b"ICC_PROFILE\0"));
        payload.remove_exif().unwrap();

        let stripped = payload.bytes().unwrap();
        let after_pixels = image::load_from_memory(stripped).unwrap().to_rgba8();

        assert!(!jpeg_contains_exif(stripped).unwrap());
        assert!(stripped.windows(4).any(|window| window == b"MPF\0"));
        assert!(
            stripped
                .windows(12)
                .any(|window| window == b"ICC_PROFILE\0")
        );
        assert_eq!(after_pixels, before_pixels);
    }

    #[test]
    fn png_exif_chunk_is_removed_without_touching_image_chunks() {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();

        png.extend_from_slice(&4_u32.to_be_bytes());
        png.extend_from_slice(b"eXIf");
        png.extend_from_slice(b"meta");
        png.extend_from_slice(&[0; 4]);
        png.extend_from_slice(&0_u32.to_be_bytes());
        png.extend_from_slice(b"IEND");
        png.extend_from_slice(&[0; 4]);

        assert!(strip_png_exif(&mut png).unwrap());

        assert!(!png.windows(4).any(|window| window == b"eXIf"));
        assert!(png.windows(4).any(|window| window == b"IEND"));
    }

    #[test]
    fn webp_exif_chunk_is_removed_and_container_size_is_updated() {
        let mut webp = b"RIFF\0\0\0\0WEBP".to_vec();

        webp.extend_from_slice(b"EXIF");
        webp.extend_from_slice(&4_u32.to_le_bytes());
        webp.extend_from_slice(b"meta");
        webp.extend_from_slice(b"VP8X");
        webp.extend_from_slice(&10_u32.to_le_bytes());
        webp.extend_from_slice(&[0x08, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let riff_size = u32::try_from(webp.len() - 8).unwrap();

        webp[4..8].copy_from_slice(&riff_size.to_le_bytes());

        assert!(strip_webp_exif(&mut webp).unwrap());

        assert!(!webp.windows(4).any(|window| window == b"EXIF"));
        assert!(webp.windows(4).any(|window| window == b"VP8X"));
        assert_eq!(webp[20] & 0x08, 0);
        assert_eq!(
            u32::from_le_bytes(webp[4..8].try_into().unwrap()),
            u32::try_from(webp.len() - 8).unwrap()
        );
    }
}
