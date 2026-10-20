//! An image you pasted into a chat input with `Ctrl`+`V`.
//!
//! The UI reads it from the clipboard and keeps it with your draft, shown in the text as its
//! placeholder (`[Image #1]`). It travels with your message in [`super::Command::Send`]; the core
//! saves it under `attachments/message-<id>/` in the data directory and adds its path to the
//! message, so agents only ever get the path.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// The largest image the app takes, in bytes: a full-screen Retina screenshot is well under it.
pub const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// The image formats the app accepts: the ones both engines' file readers can show a model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageFormat {
    /// The file extension, without the dot.
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
            Self::Webp => "webp",
        }
    }

    /// The format of `bytes`, from their signature.
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(Self::Png)
        } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
            Some(Self::Jpeg)
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some(Self::Gif)
        } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
            Some(Self::Webp)
        } else {
            None
        }
    }
}

/// An image's bytes. `Debug` prints only the length, so a command in a log line stays short; on
/// the link it serialises as a base64 string.
#[derive(Clone, PartialEq, Eq)]
pub struct ImageBytes(Vec<u8>);

impl ImageBytes {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for ImageBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ImageBytes({} bytes)", self.0.len())
    }
}

impl Serialize for ImageBytes {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use base64::Engine as _;
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for ImageBytes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use base64::Engine as _;
        let text = String::deserialize(deserializer)?;
        base64::engine::general_purpose::STANDARD
            .decode(text)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

/// One pasted image: its number in your draft (the `N` of `[Image #N]`), format and bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PastedImage {
    pub number: u32,
    pub format: ImageFormat,
    pub bytes: ImageBytes,
}

impl PastedImage {
    pub fn new(number: u32, format: ImageFormat, bytes: Vec<u8>) -> Self {
        Self {
            number,
            format,
            bytes: ImageBytes::new(bytes),
        }
    }

    /// Its placeholder in your text: `[Image #N]`.
    pub fn placeholder(&self) -> String {
        placeholder(self.number)
    }

    /// Its file name in the message's folder: `image-N.<ext>`.
    pub fn file_name(&self) -> String {
        format!("image-{}.{}", self.number, self.format.extension())
    }
}

/// The placeholder for image `number`: `[Image #N]`.
pub fn placeholder(number: u32) -> String {
    format!("[Image #{number}]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_only_the_formats_models_read() {
        assert_eq!(
            ImageFormat::sniff(b"\x89PNG\r\n\x1a\n...."),
            Some(ImageFormat::Png)
        );
        assert_eq!(
            ImageFormat::sniff(&[0xff, 0xd8, 0xff, 0xe0]),
            Some(ImageFormat::Jpeg)
        );
        assert_eq!(ImageFormat::sniff(b"GIF89a.."), Some(ImageFormat::Gif));
        assert_eq!(
            ImageFormat::sniff(b"RIFF\0\0\0\0WEBPVP8 "),
            Some(ImageFormat::Webp)
        );
        assert_eq!(ImageFormat::sniff(b"II*\0tiff"), None);
        assert_eq!(ImageFormat::sniff(b""), None);
    }

    #[test]
    fn names_follow_the_number_and_debug_hides_the_bytes() {
        let image = PastedImage::new(2, ImageFormat::Jpeg, vec![1, 2, 3]);
        assert_eq!(image.placeholder(), "[Image #2]");
        assert_eq!(image.file_name(), "image-2.jpg");
        assert_eq!(format!("{:?}", image.bytes), "ImageBytes(3 bytes)");
    }
}
