//! Guarded and bounded raster-image reads for MCP image content.
use crate::{NovaError, NovaResult, WorkspaceRegistry};
use std::{fs, io::Read};

const IMAGE_MAX: usize = 2 * 1024 * 1024;

pub struct ImageDocument {
    pub path: String,
    pub mime_type: &'static str,
    pub bytes: Vec<u8>,
}

fn image_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

impl WorkspaceRegistry {
    pub fn view_image(&self, id: Option<&str>, relative: &str) -> NovaResult<ImageDocument> {
        let root = self.workspace(id)?.root;
        let target = self.resolve(id, relative, false)?;
        let metadata = fs::metadata(&target)?;
        if !metadata.is_file() {
            return Err(NovaError::InvalidInput(
                "Image target must be a regular file".into(),
            ));
        }
        if metadata.len() > IMAGE_MAX as u64 {
            return Err(NovaError::TooLarge(IMAGE_MAX));
        }
        let mut bytes = Vec::new();
        fs::File::open(&target)?
            .take((IMAGE_MAX + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > IMAGE_MAX {
            return Err(NovaError::TooLarge(IMAGE_MAX));
        }
        let mime_type = image_type(&bytes).ok_or_else(|| {
            NovaError::InvalidInput("Unsupported image signature; PNG/JPEG/GIF/WebP only".into())
        })?;
        Ok(ImageDocument {
            path: target
                .strip_prefix(&root)
                .map_err(|_| NovaError::OutsideAllowedRoot)?
                .to_string_lossy()
                .into_owned(),
            mime_type,
            bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_image_signature_and_path_limits() {
        let temp = tempfile::tempdir().expect("root");
        let state = WorkspaceRegistry::new(temp.path(), &[]).expect("registry");
        fs::write(temp.path().join("image.png"), b"\x89PNG\r\n\x1a\nfixture").expect("image");
        let result = state.view_image(None, "image.png").expect("image");
        assert_eq!(result.mime_type, "image/png");
        assert_eq!(result.bytes.len(), 15);
        fs::write(temp.path().join("fake.png"), b"not an image").expect("fake");
        assert!(state.view_image(None, "fake.png").is_err());
        assert!(state.view_image(None, "../outside.png").is_err());
        fs::write(temp.path().join(".env"), b"\x89PNG\r\n\x1a\n").expect("protected");
        assert!(matches!(
            state.view_image(None, ".env"),
            Err(NovaError::BlockedPath)
        ));
        fs::write(temp.path().join("huge.png"), vec![0x89; IMAGE_MAX + 1]).expect("large");
        assert!(matches!(
            state.view_image(None, "huge.png"),
            Err(NovaError::TooLarge(_))
        ));
    }
}
