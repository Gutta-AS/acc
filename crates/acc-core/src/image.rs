//! Images in user messages, by reference.
//!
//! Drivers used to show an image-only prompt as the text `[image]` and drop
//! images next to text without a trace. Now a user message with images
//! carries ACC's references to them in its [`Event::ItemStarted`] `native`,
//! as `{"images": [ImageRef, ...]}` (Codex keeps its own item JSON there and
//! gains the `images` key). The bytes stay out of events, which are
//! resent and logged; a client that sent the prompt matches its own
//! attachments by `sha256`. Messages without images are unchanged.
//!
//! [`Event::ItemStarted`]: crate::event::Event::ItemStarted

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::command::Attachment;
use crate::wire::str_at;

/// One image a user message carried, without its bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRef {
    /// `image/png` and so on, when the harness says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    /// Decoded size, when the bytes were inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// Lowercase hex SHA-256 of the decoded bytes, when they were inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// The file on the host, when the harness took the image by path
    /// (Codex `localImage`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl ImageRef {
    /// From decoded bytes.
    pub fn of_bytes(media_type: Option<&str>, bytes: &[u8]) -> Self {
        let mut hex = String::with_capacity(64);
        for b in Sha256::digest(bytes) {
            hex.push(char::from(HEX[usize::from(b >> 4)]));
            hex.push(char::from(HEX[usize::from(b & 0xf)]));
        }
        Self {
            media_type: media_type.filter(|m| !m.is_empty()).map(str::to_owned),
            bytes: Some(bytes.len() as u64),
            sha256: Some(hex),
            path: None,
        }
    }

    /// From standard base64; invalid base64 gives a ref with only the type.
    pub fn of_base64(media_type: Option<&str>, data: &str) -> Self {
        match base64::engine::general_purpose::STANDARD.decode(data.trim()) {
            Ok(bytes) => Self::of_bytes(media_type, &bytes),
            Err(_) => Self {
                media_type: media_type.map(str::to_owned),
                ..Self::default()
            },
        }
    }

    /// From a `data:<type>;base64,<data>` URL. Other URLs keep no bytes.
    pub fn of_url(url: &str) -> Self {
        if let Some((head, data)) = url.strip_prefix("data:").and_then(|u| u.split_once(','))
            && let Some(media_type) = head.strip_suffix(";base64")
        {
            return Self::of_base64(Some(media_type), data);
        }
        Self::default()
    }

    pub fn of_attachment(a: &Attachment) -> Self {
        Self::of_bytes(Some(&a.media_type), &a.bytes())
    }

    /// An image block in any harness's spelling: Anthropic
    /// (`source.type` `base64` with `media_type` and `data`), pi and ACP
    /// (`data` and `mimeType`), Codex (`image` with `url` or `image_url`,
    /// `localImage` with `path`). `None` when it isn't an image block.
    pub fn of_block(block: &Value) -> Option<Self> {
        match str_at(block, "type")? {
            "image" => {
                if let Some(source) = block.get("source") {
                    let media_type = str_at(source, "media_type");
                    return Some(match str_at(source, "type") {
                        Some("base64") => {
                            Self::of_base64(media_type, str_at(source, "data").unwrap_or_default())
                        }
                        _ => Self {
                            media_type: media_type.map(str::to_owned),
                            ..Self::default()
                        },
                    });
                }
                if let Some(data) = str_at(block, "data") {
                    let media_type = str_at(block, "mimeType").or_else(|| str_at(block, "media_type"));
                    return Some(Self::of_base64(media_type, data));
                }
                let url = str_at(block, "url").or_else(|| str_at(block, "image_url"));
                Some(url.map(Self::of_url).unwrap_or_default())
            }
            "localImage" => Some(Self {
                path: str_at(block, "path").map(str::to_owned),
                ..Self::default()
            }),
            _ => None,
        }
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// The image refs among a message's content blocks.
pub fn refs_in(content: Option<&Value>) -> Vec<ImageRef> {
    content
        .and_then(Value::as_array)
        .map(|blocks| blocks.iter().filter_map(ImageRef::of_block).collect())
        .unwrap_or_default()
}

/// A user message's `native`: `{"images": [...]}`, or null without images.
pub fn native(refs: &[ImageRef]) -> Value {
    if refs.is_empty() {
        Value::Null
    } else {
        json!({ "images": refs })
    }
}

/// The refs in an item event's `native`, for clients.
pub fn refs_of(native: &Value) -> Vec<ImageRef> {
    native
        .get("images")
        .and_then(|v| Vec::<ImageRef>::deserialize(v).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn every_spelling_gives_the_same_ref() {
        let png = ImageRef::of_bytes(Some("image/png"), b"PNG");
        assert_eq!(png.bytes, Some(3));
        assert_eq!(png.sha256.as_deref().unwrap().len(), 64);
        let anthropic =
            json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "UE5H"}});
        let pi = json!({"type": "image", "mimeType": "image/png", "data": "UE5H"});
        let codex = json!({"type": "image", "url": "data:image/png;base64,UE5H"});
        let codex_next = json!({"type": "image", "image_url": "data:image/png;base64,UE5H"});
        for block in [anthropic, pi, codex, codex_next] {
            assert_eq!(ImageRef::of_block(&block).unwrap(), png, "{block}");
        }
        let a = Attachment::from_bytes("image/png", b"PNG");
        assert_eq!(ImageRef::of_attachment(&a), png);
    }

    #[test]
    fn sha256_is_lowercase_hex_of_the_bytes() {
        // sha256("abc")
        let r = ImageRef::of_bytes(None, b"abc");
        assert_eq!(
            r.sha256.as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
    }

    #[test]
    fn paths_urls_and_non_images() {
        let local = ImageRef::of_block(&json!({"type": "localImage", "path": "/w/a.png"})).unwrap();
        assert_eq!(local.path.as_deref(), Some("/w/a.png"));
        assert_eq!(local.sha256, None);
        let remote = ImageRef::of_block(&json!({"type": "image", "url": "https://x/a.png"})).unwrap();
        assert_eq!(remote, ImageRef::default());
        assert_eq!(ImageRef::of_block(&json!({"type": "text", "text": "hi"})), None);
        let bad = ImageRef::of_base64(Some("image/png"), "not base64!");
        assert_eq!((bad.media_type.as_deref(), bad.bytes), (Some("image/png"), None));
    }

    #[test]
    fn native_round_trips_and_is_null_without_images() {
        assert!(native(&[]).is_null());
        let refs = vec![ImageRef::of_bytes(Some("image/png"), b"PNG")];
        let n = native(&refs);
        assert_eq!(refs_of(&n), refs);
        assert!(!n.to_string().contains("UE5H"), "no bytes in events");
        let content = json!([{"type": "text", "text": "look"}, {"type": "image", "mimeType": "image/png", "data": "UE5H"}]);
        assert_eq!(refs_in(Some(&content)), refs);
        assert!(refs_of(&json!({"type": "userMessage"})).is_empty());
    }
}
