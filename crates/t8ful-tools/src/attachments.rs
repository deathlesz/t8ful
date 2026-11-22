//! Session-owned image storage behind an injected interface.
//!
//! A tool may return an image, but `t8ful-tools` has no notion of a session and
//! must not learn one: the attachment directory belongs to the journal, whose id
//! and state paths live in the binary crate. The provider reads an attachment
//! from disk when it builds a request, so the bytes have to be durable — a
//! temporary file deleted on drop would break resume.
//!
//! [`AttachmentStore`] is the seam. `t8ful` implements it by delegating to the
//! same validation and storage path the clipboard and `@`-mention attachments
//! already use, so there is one validation path, not two.

use t8ful_core::content::ImageInput;
use t8ful_core::tool::ToolError;

/// Durable, session-owned storage for images a tool returns.
pub trait AttachmentStore: Send + Sync + std::fmt::Debug {
    /// Validate the bytes and copy them into session storage.
    ///
    /// `max_encoded_bytes` bounds the base64 form the provider will build, the
    /// same limit submitted images obey. The implementation sniffs the real
    /// format, so a caller's guess cannot decide how the bytes are stored.
    fn store(&self, bytes: &[u8], max_encoded_bytes: usize) -> Result<ImageInput, ToolError>;
}

/// A store that refuses everything, for a run with no session.
///
/// Tests and one-shot runs compose the builtin registry without a journal, so
/// reading an image stays an ordinary error there instead of a panic.
#[derive(Debug, Default)]
pub struct NoAttachments;

impl AttachmentStore for NoAttachments {
    fn store(&self, _bytes: &[u8], _max_encoded_bytes: usize) -> Result<ImageInput, ToolError> {
        Err(ToolError::Failed(
            "image attachments require a saved session".to_owned(),
        ))
    }
}

/// The image formats `read_file` recognises, detected from magic bytes.
///
/// Kept here rather than behind the `image` crate: `t8ful-tools` must stay free
/// of a decoder dependency for a feature that only needs dimensions. The
/// sniffing is a deliberate duplicate of `t8ful::images::detect`, which is
/// authoritative for validation; this copy only decides whether a file is worth
/// handing to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SniffedImage {
    Png { width: u32, height: u32 },
    Gif { width: u32, height: u32 },
    Webp { width: u32, height: u32 },
    Jpeg { width: u32, height: u32 },
}

impl SniffedImage {
    pub fn media_type(self) -> &'static str {
        match self {
            Self::Png { .. } => "image/png",
            Self::Jpeg { .. } => "image/jpeg",
            Self::Webp { .. } => "image/webp",
            Self::Gif { .. } => "image/gif",
        }
    }

    pub fn dimensions(self) -> (u32, u32) {
        match self {
            Self::Png { width, height }
            | Self::Gif { width, height }
            | Self::Webp { width, height }
            | Self::Jpeg { width, height } => (width, height),
        }
    }
}

/// Bytes a base64 encoding of `raw` will occupy.
///
/// The image limit bounds the encoded form because that is what the provider
/// transmits, so this is the number every check has to compare.
pub fn encoded_size(raw: u64) -> u64 {
    raw.div_ceil(3) * 4
}

/// The container a file starts with, even when its dimensions could not be read.
///
/// Used only to explain a failure: a JPEG whose frame header sits past the sniff
/// window is still an image, and reporting it as invalid UTF-8 would send the
/// caller looking for the wrong problem.
pub fn image_magic(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("PNG");
    }

    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("GIF");
    }

    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return Some("WebP");
    }

    if bytes.starts_with(b"\xff\xd8") {
        return Some("JPEG");
    }

    None
}

/// Recognise a PNG, GIF, WebP, or JPEG from its leading bytes.
///
/// Returns `None` for anything else, which includes every text file and every
/// other binary, so the caller keeps its existing not-UTF-8 error.
pub fn sniff_image(bytes: &[u8]) -> Option<SniffedImage> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        if bytes.len() < 24 || &bytes[12..16] != b"IHDR" {
            return None;
        }

        let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
        let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);

        return Some(SniffedImage::Png { width, height });
    }

    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        if bytes.len() < 10 {
            return None;
        }

        let width = u16::from_le_bytes(bytes[6..8].try_into().ok()?) as u32;
        let height = u16::from_le_bytes(bytes[8..10].try_into().ok()?) as u32;

        return Some(SniffedImage::Gif { width, height });
    }

    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return webp_dimensions(bytes);
    }

    if bytes.starts_with(b"\xff\xd8") {
        return jpeg_dimensions(bytes);
    }

    None
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<SniffedImage> {
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

            return Some(SniffedImage::Jpeg { width, height });
        }

        index += length;
    }

    None
}

fn webp_dimensions(bytes: &[u8]) -> Option<SniffedImage> {
    if bytes.len() < 30 {
        return None;
    }

    match &bytes[12..16] {
        b"VP8X" => {
            let width = 1 + u32::from_le_bytes([bytes[24], bytes[25], bytes[26], 0]);
            let height = 1 + u32::from_le_bytes([bytes[27], bytes[28], bytes[29], 0]);

            Some(SniffedImage::Webp { width, height })
        }
        b"VP8 " => {
            let width = u16::from_le_bytes([bytes[26], bytes[27]]) & 0x3fff;
            let height = u16::from_le_bytes([bytes[28], bytes[29]]) & 0x3fff;

            Some(SniffedImage::Webp {
                width: u32::from(width),
                height: u32::from(height),
            })
        }
        b"VP8L" => {
            let bits = u32::from_le_bytes([bytes[21], bytes[22], bytes[23], bytes[24]]);
            let width = (bits & 0x3fff) + 1;
            let height = ((bits >> 14) & 0x3fff) + 1;

            Some(SniffedImage::Webp { width, height })
        }
        _ => None,
    }
}

/// The caption a tool result shows for one image it returned.
///
/// The model needs to know what the picture is, and the transcript needs the same
/// line whether it came from the wire or from a resumed journal.
pub fn caption(path: &std::path::Path, image: &ImageInput) -> String {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    format!(
        "{} · {} · {}x{} · {}",
        name,
        image.media_type,
        image.width,
        image.height,
        human_bytes(image.bytes)
    )
}

/// Bytes in the largest unit that keeps the number small.
///
/// The same three-tier rule the transcript previews use, so one image is
/// described the same way in a card and in a tool result.
fn human_bytes(bytes: u64) -> String {
    match bytes {
        0..=1023 => format!("{bytes} B"),
        1024..=1_048_575 => format!("{:.1} KiB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0)),
    }
}

#[cfg(test)]
mod tests {
    use super::{SniffedImage, caption, human_bytes, sniff_image};
    use t8ful_core::content::ImageInput;

    fn png(width: u32, height: u32) -> Vec<u8> {
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
    fn recognises_every_supported_format_and_reads_dimensions() {
        let mut gif = b"GIF89a".to_vec();

        gif.extend_from_slice(&7_u16.to_le_bytes());
        gif.extend_from_slice(&9_u16.to_le_bytes());
        gif.extend_from_slice(&[0; 4]);

        let mut webp = b"RIFF".to_vec();

        webp.extend_from_slice(&0_u32.to_le_bytes());
        webp.extend_from_slice(b"WEBP");
        // VP8X: four-byte chunk size, one flags byte plus three reserved, then
        // the canvas width and height, each stored as the value minus one.
        webp.extend_from_slice(b"VP8X");
        webp.extend_from_slice(&0_u32.to_le_bytes());
        webp.extend_from_slice(&[0, 0, 0, 0]);
        webp.extend_from_slice(&[4, 0, 0]);
        webp.extend_from_slice(&[2, 0, 0]);

        let cases: Vec<(Vec<u8>, SniffedImage, &'static str)> = vec![
            (
                png(1200, 800),
                SniffedImage::Png {
                    width: 1200,
                    height: 800,
                },
                "image/png",
            ),
            (
                gif,
                SniffedImage::Gif {
                    width: 7,
                    height: 9,
                },
                "image/gif",
            ),
            (
                webp,
                SniffedImage::Webp {
                    width: 5,
                    height: 3,
                },
                "image/webp",
            ),
        ];

        for (bytes, expected, media_type) in cases {
            let sniffed = sniff_image(&bytes).unwrap_or_else(|| panic!("{media_type}"));

            assert_eq!(sniffed, expected, "{media_type}");
            assert_eq!(sniffed.media_type(), media_type);
        }
    }

    #[test]
    fn a_text_file_and_a_foreign_binary_are_not_images() {
        assert_eq!(sniff_image(b"fn main() {}\n"), None);
        assert_eq!(sniff_image(b"\x7fELF\x02\x01\x01"), None);
        assert_eq!(sniff_image(&[]), None);
        // A truncated PNG header names no dimensions, so it is not an image.
        assert_eq!(sniff_image(b"\x89PNG\r\n\x1a\n"), None);
    }

    #[test]
    fn the_caption_names_the_file_its_format_and_its_size() {
        let image = ImageInput::new(
            "image/png",
            std::path::PathBuf::from("/state/attachments/shot.png"),
            1200,
            800,
            34_816,
        );

        assert_eq!(
            caption(std::path::Path::new("/tmp/shot.png"), &image),
            "shot.png · image/png · 1200x800 · 34.0 KiB"
        );
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(20 * 1024 * 1024), "20.0 MiB");
    }
}
