//! Image validation and session-owned attachment storage.

use std::io::Write;
use std::path::Path;

use t8ful_core::content::ImageInput;

use crate::session::SessionId;
use crate::state::{StatePaths, ensure_private_dir};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Webp,
    Gif,
}

impl ImageFormat {
    pub fn media_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
            Self::Gif => "image/gif",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
            Self::Gif => "gif",
        }
    }

    pub fn from_media_type(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "image/png" => Some(Self::Png),
            "image/jpeg" | "image/jpg" => Some(Self::Jpeg),
            "image/webp" => Some(Self::Webp),
            "image/gif" => Some(Self::Gif),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedImage {
    pub format: ImageFormat,
    pub width: u32,
    pub height: u32,
}

pub fn encoded_size(raw_bytes: usize) -> usize {
    raw_bytes.div_ceil(3) * 4
}

/// Validate a PNG, JPEG, WebP, or GIF image and enforce the encoded-size limit.
pub fn validate(
    bytes: &[u8],
    declared_media_type: Option<&str>,
    max_encoded_bytes: usize,
) -> Result<ValidatedImage, String> {
    if bytes.is_empty() {
        return Err("the image file is empty".to_owned());
    }

    if encoded_size(bytes.len()) > max_encoded_bytes {
        return Err(format!(
            "the base64-encoded image would be {} bytes, above the configured limit of {} bytes",
            encoded_size(bytes.len()),
            max_encoded_bytes
        ));
    }

    let (format, width, height) = detect(bytes)?;

    if let Some(declared) = declared_media_type {
        match ImageFormat::from_media_type(declared) {
            Some(declared) if declared == format => {}
            Some(_) => {
                return Err(format!(
                    "the image data is {}, but the declared type is {declared}",
                    format.media_type()
                ));
            }
            None => {
                return Err(format!("unsupported declared image type `{declared}`"));
            }
        }
    }

    if width == 0 || height == 0 {
        return Err("the image has zero width or height".to_owned());
    }

    Ok(ValidatedImage {
        format,
        width,
        height,
    })
}

fn detect(bytes: &[u8]) -> Result<(ImageFormat, u32, u32), String> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        if bytes.len() < 24 || &bytes[12..16] != b"IHDR" {
            return Err("malformed PNG header".to_owned());
        }

        let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
        let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());

        return Ok((ImageFormat::Png, width, height));
    }

    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        if bytes.len() < 10 {
            return Err("malformed GIF header".to_owned());
        }

        let width = u16::from_le_bytes(bytes[6..8].try_into().unwrap()) as u32;
        let height = u16::from_le_bytes(bytes[8..10].try_into().unwrap()) as u32;

        return Ok((ImageFormat::Gif, width, height));
    }

    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return webp_dimensions(bytes);
    }

    if bytes.starts_with(b"\xff\xd8") {
        return jpeg_dimensions(bytes);
    }

    Err("unsupported image format: expected PNG, JPEG, WebP, or GIF".to_owned())
}

fn jpeg_dimensions(bytes: &[u8]) -> Result<(ImageFormat, u32, u32), String> {
    let mut index = 2;

    while index + 4 <= bytes.len() {
        if bytes[index] != 0xff {
            index += 1;

            continue;
        }

        let marker = bytes[index + 1];
        index += 2;

        if matches!(marker, 0xd8 | 0xd9) {
            continue;
        }

        if index + 2 > bytes.len() {
            break;
        }

        let length = u16::from_be_bytes([bytes[index], bytes[index + 1]]) as usize;

        if length < 2 || index + length > bytes.len() {
            break;
        }

        if matches!(
            marker,
            0xc0 | 0xc1
                | 0xc2
                | 0xc3
                | 0xc5
                | 0xc6
                | 0xc7
                | 0xc9
                | 0xca
                | 0xcb
                | 0xcd
                | 0xce
                | 0xcf
        ) {
            if length < 7 {
                break;
            }

            let height = u16::from_be_bytes([bytes[index + 3], bytes[index + 4]]) as u32;
            let width = u16::from_be_bytes([bytes[index + 5], bytes[index + 6]]) as u32;

            return Ok((ImageFormat::Jpeg, width, height));
        }

        index += length;
    }

    Err("malformed JPEG image: no frame header".to_owned())
}

fn webp_dimensions(bytes: &[u8]) -> Result<(ImageFormat, u32, u32), String> {
    if bytes.len() < 30 {
        return Err("malformed WebP image".to_owned());
    }

    match &bytes[12..16] {
        b"VP8X" => {
            let width = 1 + u32::from_le_bytes([bytes[24], bytes[25], bytes[26], 0]);
            let height = 1 + u32::from_le_bytes([bytes[27], bytes[28], bytes[29], 0]);

            Ok((ImageFormat::Webp, width, height))
        }
        b"VP8 " => {
            let width = u16::from_le_bytes([bytes[26], bytes[27]]) & 0x3fff;
            let height = u16::from_le_bytes([bytes[28], bytes[29]]) & 0x3fff;

            Ok((ImageFormat::Webp, u32::from(width), u32::from(height)))
        }
        b"VP8L" => {
            let bits = u32::from_le_bytes([bytes[21], bytes[22], bytes[23], bytes[24]]);
            let width = (bits & 0x3fff) + 1;
            let height = ((bits >> 14) & 0x3fff) + 1;

            Ok((ImageFormat::Webp, width, height))
        }
        _ => Err("unsupported WebP image variant".to_owned()),
    }
}

/// Copy validated image bytes into private session storage.
pub fn store(
    paths: &StatePaths,
    session: &SessionId,
    bytes: &[u8],
    image: &ValidatedImage,
) -> Result<ImageInput, String> {
    let dir = paths.attachment_dir(session.as_str());

    ensure_private_dir(&dir).map_err(|error| {
        format!(
            "cannot create the session attachment directory {}: {error}",
            dir.display()
        )
    })?;

    // A random same-directory name avoids two stores in one millisecond
    // aliasing one file. `tempfile` creates the file exclusively at 0600.
    let mut temp = tempfile::Builder::new()
        .prefix("image-")
        .suffix(&format!(".{}", image.format.extension()))
        .tempfile_in(&dir)
        .map_err(|error| format!("cannot create the attachment directory entry: {error}"))?;

    temp.write_all(bytes)
        .map_err(|error| format!("cannot write the attachment: {error}"))?;
    temp.flush()
        .map_err(|error| format!("cannot flush the attachment: {error}"))?;

    let path = temp
        .into_temp_path()
        .keep()
        .map_err(|error| format!("cannot persist the attachment: {error}"))?;

    crate::state::ensure_private_file(&path)
        .map_err(|error| format!("cannot secure the attachment {}: {error}", path.display()))?;

    Ok(ImageInput::new(
        image.format.media_type(),
        path,
        image.width,
        image.height,
        bytes.len() as u64,
    ))
}

pub fn attach_path(
    paths: &StatePaths,
    session: &SessionId,
    path: &Path,
    max_encoded_bytes: usize,
) -> Result<ImageInput, String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("cannot read the image {}: {error}", path.display()))?;
    let image = validate(&bytes, None, max_encoded_bytes)?;

    store(paths, session, &bytes, &image)
}

pub fn attach_bytes(
    paths: &StatePaths,
    session: &SessionId,
    bytes: &[u8],
    media_type: Option<&str>,
    max_encoded_bytes: usize,
) -> Result<ImageInput, String> {
    let image = validate(bytes, media_type, max_encoded_bytes)?;

    store(paths, session, bytes, &image)
}

#[cfg(test)]
mod tests {
    use super::{ImageFormat, encoded_size, validate};

    pub(super) fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();

        bytes.extend_from_slice(&13_u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes.extend_from_slice(b"IEND");

        bytes
    }

    #[test]
    fn validates_png_dimensions_and_media_type() {
        let image = validate(&png(640, 480), None, usize::MAX).unwrap();

        assert_eq!(image.format, ImageFormat::Png);
        assert_eq!((image.width, image.height), (640, 480));
        assert!(validate(&png(1, 1), Some("image/jpeg"), usize::MAX).is_err());
    }

    #[test]
    fn enforces_the_encoded_size_limit() {
        let bytes = png(1, 1);
        let limit = encoded_size(bytes.len());

        assert!(validate(&bytes, None, limit).is_ok());
        assert!(validate(&bytes, None, limit - 1).is_err());
    }

    #[test]
    fn rejects_unknown_and_malformed_data() {
        assert!(validate(b"not an image", None, usize::MAX).is_err());
        assert!(validate(&[], None, usize::MAX).is_err());
        assert!(validate(b"\x89PNG\r\n\x1a\n", None, usize::MAX).is_err());
    }
}

#[cfg(test)]
mod storage_tests {
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::{attach_bytes, attach_path, validate};
    use crate::session::SessionId;
    use crate::state::StatePaths;

    #[test]
    fn attachments_are_copied_into_private_session_storage() {
        let dir = TempDir::new().unwrap();
        let paths = StatePaths {
            root: dir.path().join("state"),
            sessions: dir.path().join("state/sessions"),
            usage: dir.path().join("state/usage"),
        };
        let source = dir.path().join("source.png");
        let bytes = super::tests::png(3, 2);

        std::fs::write(&source, &bytes).unwrap();
        let session = SessionId::parse("session-1").unwrap();
        let image = attach_path(&paths, &session, &source, usize::MAX).unwrap();

        assert_eq!(image.width, 3);
        assert_eq!(image.height, 2);
        assert_eq!(std::fs::read(&image.path).unwrap(), bytes);
        assert_eq!(
            std::fs::metadata(&image.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(validate(&bytes, Some("image/png"), usize::MAX).is_ok());
    }

    #[test]
    fn rapid_attachments_get_distinct_files_and_keep_their_bytes() {
        let dir = TempDir::new().unwrap();
        let paths = StatePaths {
            root: dir.path().join("state"),
            sessions: dir.path().join("state/sessions"),
            usage: dir.path().join("state/usage"),
        };
        let session = SessionId::parse("session-1").unwrap();
        let first_bytes = super::tests::png(1, 1);
        let second_bytes = super::tests::png(2, 2);
        let mut seen = std::collections::BTreeSet::new();

        for index in 0..32 {
            let bytes = if index % 2 == 0 {
                &first_bytes
            } else {
                &second_bytes
            };
            let image = attach_bytes(&paths, &session, bytes, Some("image/png"), usize::MAX)
                .expect("attach");

            assert!(
                seen.insert(image.path.clone()),
                "two attachments aliased {}",
                image.path.display()
            );
            assert_eq!(std::fs::read(&image.path).unwrap(), *bytes);
        }
    }
}
