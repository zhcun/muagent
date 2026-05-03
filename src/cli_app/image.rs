//! Build user-facing `Content` from a prompt + optional local image paths.

use std::path::Path;

use crate::core::types::{Content, ContentPart};

pub fn user_content(prompt: &str, image_paths: &[String]) -> Result<Content, String> {
    if image_paths.is_empty() {
        return Ok(Content::text(prompt));
    }

    let mut parts = Vec::with_capacity(image_paths.len() + 1);
    parts.push(ContentPart::Text {
        text: prompt.to_string(),
    });
    for image in image_paths {
        let path = Path::new(image);
        let bytes = std::fs::read(path).map_err(|e| format!("read image `{image}`: {e}"))?;
        parts.push(ContentPart::Image {
            uri: None,
            b64: Some(base64_encode(&bytes)),
            mime: image_mime(path)?,
        });
    }
    Ok(Content::Parts(parts))
}

pub fn image_mime(path: &Path) -> Result<String, String> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "png" => Ok("image/png".into()),
        "jpg" | "jpeg" => Ok("image/jpeg".into()),
        "webp" => Ok("image/webp".into()),
        "gif" => Ok("image/gif".into()),
        _ => Err(format!(
            "unsupported image extension for `{}`; supported: png, jpg, jpeg, webp, gif",
            path.display()
        )),
    }
}

pub fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i];
        let b1 = bytes.get(i + 1).copied().unwrap_or(0);
        let b2 = bytes.get(i + 2).copied().unwrap_or(0);
        let n = ((b0 as u32) << 16) | ((b1 as u32) << 8) | b2 as u32;

        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        if i + 1 < bytes.len() {
            out.push(TABLE[((n >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if i + 2 < bytes.len() {
            out.push(TABLE[(n & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        i += 3;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn local_image_arg_is_sent_as_inline_b64_not_local_uri() {
        let path = std::env::temp_dir().join(format!("muagent-cli-image-{}.png", Uuid::new_v4()));
        std::fs::write(&path, [1_u8, 2, 3]).unwrap();

        let content = user_content("read it", &[path.display().to_string()]).unwrap();
        let Content::Parts(parts) = content else {
            panic!("expected multipart content");
        };
        let ContentPart::Image { uri, b64, mime } = &parts[1] else {
            panic!("expected image part");
        };
        assert!(uri.is_none());
        assert_eq!(b64.as_deref(), Some("AQID"));
        assert_eq!(mime, "image/png");

        let _ = std::fs::remove_file(path);
    }
}
