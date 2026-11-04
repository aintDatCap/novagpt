//! Independent, bounded implementation of the ChromeDriver launch-block
//! neutralization that upstream performs.
//!
//! Upstream replaces the JavaScript block beginning `{window.cdc…` inside the
//! ChromeDriver executable with an inert, same-length expression. The block is
//! ChromeDriver's injected automation signal. NovaGPT performs the same
//! *functional* transformation without copying GPL implementation text:
//! it locates a bounded block, replaces it with a NovaGPT marker expression,
//! pads with spaces to preserve file offsets, and verifies the result.
//!
//! This is a binary rewrite of a downloaded ChromeDriver. It does not modify
//! Chrome, the user's profile, cookies, or any browser identity value.
use super::{error::UpstreamError, platform::ChromeVersion};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::Path,
};

/// Marker that identifies a binary patched by this crate. Deliberately not
/// the upstream marker: NovaGPT manages only binaries it installed itself and
/// refuses to silently adopt a binary transformed by another implementation.
const MARKER: &[u8] = b"novagpt-chromedriver-patch";
const FOREIGN_MARKER: &[u8] = b"undetected chromedriver";
const INJECTION_PREFIX: &[u8] = b"{window.cdc";
const INJECTION_SUFFIX: &[u8] = b";}";
const REPLACEMENT: &[u8] = b"{/*novagpt-chromedriver-patch*/}";

const MAX_BINARY_BYTES: u64 = 256 * 1024 * 1024;
const MAX_INJECTION_BLOCK: usize = 64 * 1024;
const MAX_VERSION_PREFIX: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PatchStatus {
    Patched,
    AlreadyPatched,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PatchReport {
    pub status: PatchStatus,
    pub blocks: usize,
}

/// Read the version embedded in a ChromeDriver binary, following the same
/// stable `platform_handle\0content\0<version>` marker upstream scans for.
/// Reading the binary is preferred over executing freshly downloaded code.
pub fn binary_version(path: &Path) -> Result<ChromeVersion, UpstreamError> {
    let mut file = File::open(path).map_err(|_| UpstreamError::Io)?;
    let metadata = file.metadata().map_err(|_| UpstreamError::Io)?;
    if !metadata.is_file() || metadata.len() > MAX_BINARY_BYTES {
        return Err(UpstreamError::Unpatchable);
    }
    let capacity = usize::try_from(metadata.len().min(MAX_VERSION_PREFIX as u64))
        .map_err(|_| UpstreamError::Unpatchable)?;
    let mut prefix = vec![0_u8; capacity];
    let read = file
        .read(&mut prefix)
        .map_err(|_| UpstreamError::Unpatchable)?;
    prefix.truncate(read);
    find_version(&prefix).ok_or(UpstreamError::Unpatchable)
}

fn find_version(bytes: &[u8]) -> Option<ChromeVersion> {
    const NEEDLE: &[u8] = b"platform_handle\x00content\x00";
    let start = bytes.windows(NEEDLE.len()).position(|w| w == NEEDLE)? + NEEDLE.len();
    let rest = &bytes[start..];
    let end = rest
        .iter()
        .position(|byte| !byte.is_ascii_digit() && *byte != b'.')
        .unwrap_or(rest.len())
        .min(48);
    std::str::from_utf8(&rest[..end]).ok()?.parse().ok()
}

/// Patch a ChromeDriver binary in place. The file is read fully, rewritten to
/// a sibling temporary file, synchronously flushed, and atomically renamed.
/// A crash can therefore leave the original or the complete replacement, never
/// a half-written executable.
pub fn patch_driver(path: &Path) -> Result<PatchReport, UpstreamError> {
    patch_driver_with_marker(path, MARKER, FOREIGN_MARKER, REPLACEMENT)
}

fn patch_driver_with_marker(
    path: &Path,
    marker: &[u8],
    foreign_marker: &[u8],
    replacement: &[u8],
) -> Result<PatchReport, UpstreamError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| UpstreamError::Io)?;
    if !metadata.is_file() || metadata.len() > MAX_BINARY_BYTES {
        return Err(UpstreamError::Unpatchable);
    }
    let mut file = File::open(path).map_err(|_| UpstreamError::Io)?;
    let mut content = Vec::with_capacity(
        usize::try_from(metadata.len()).map_err(|_| UpstreamError::Unpatchable)?,
    );
    file.read_to_end(&mut content)
        .map_err(|_| UpstreamError::Io)?;
    if content.len() > MAX_BINARY_BYTES as usize {
        return Err(UpstreamError::TooLarge);
    }

    if content.windows(marker.len()).any(|window| window == marker) {
        return Ok(PatchReport {
            status: PatchStatus::AlreadyPatched,
            blocks: 0,
        });
    }
    if content
        .windows(foreign_marker.len())
        .any(|window| window == foreign_marker)
    {
        // A binary patched by upstream has unknown provenance. Never mutate
        // it further just because the byte pattern is recognisable.
        return Err(UpstreamError::ForeignPatch);
    }

    let mut blocks = 0_usize;
    let mut offset = 0_usize;
    while let Some(relative) = content[offset..]
        .windows(INJECTION_PREFIX.len())
        .position(|window| window == INJECTION_PREFIX)
    {
        let start = offset + relative;
        let search_end = (start + MAX_INJECTION_BLOCK).min(content.len());
        let Some(suffix) = content[start..search_end]
            .windows(INJECTION_SUFFIX.len())
            .position(|window| window == INJECTION_SUFFIX)
        else {
            return Err(UpstreamError::Unpatchable);
        };
        let end = start + suffix + INJECTION_SUFFIX.len();
        let original_length = end - start;
        if original_length < replacement.len() {
            return Err(UpstreamError::Unpatchable);
        }
        let mut replacement_block = replacement.to_vec();
        replacement_block.resize(original_length, b' ');
        content[start..end].copy_from_slice(&replacement_block);
        blocks += 1;
        offset = end;
    }
    if blocks == 0 {
        return Err(UpstreamError::Unpatchable);
    }
    if content
        .windows(INJECTION_PREFIX.len())
        .any(|w| w == INJECTION_PREFIX)
        || !content.windows(marker.len()).any(|window| window == marker)
    {
        return Err(UpstreamError::Unpatchable);
    }

    let parent = path.parent().ok_or(UpstreamError::Unpatchable)?;
    let file_name = path.file_name().ok_or(UpstreamError::Unpatchable)?;
    let temp = parent.join(format!(
        ".{}.novagpt-patch-{}",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    let result = (|| {
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|_| UpstreamError::Io)?;
        output.write_all(&content).map_err(|_| UpstreamError::Io)?;
        output.sync_all().map_err(|_| UpstreamError::Io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temp, fs::Permissions::from_mode(0o755))
                .map_err(|_| UpstreamError::Io)?;
        }
        fs::rename(&temp, path).map_err(|_| UpstreamError::Io)?;
        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(PatchReport {
            status: PatchStatus::Patched,
            blocks,
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Exact-version presence check used for freshly extracted ChromeDriver
/// builds. Recent ChromeDriver binaries no longer contain the single
/// `platform_handle\0content\0` record upstream scans for, so installation
/// verifies the exact release string instead of executing the new binary.
pub fn contains_version(bytes: &[u8], version: &ChromeVersion) -> bool {
    let needle = version.to_string();
    let needle = needle.as_bytes();
    bytes
        .windows(needle.len())
        .enumerate()
        .any(|(index, window)| {
            window == needle
                && index
                    .checked_sub(1)
                    .and_then(|before| bytes.get(before))
                    .is_none_or(|byte| !byte.is_ascii_digit() && *byte != b'.')
                && bytes
                    .get(index + needle.len())
                    .is_none_or(|byte| !byte.is_ascii_digit() && *byte != b'.')
        })
}

pub fn is_patched(path: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.is_file() || metadata.len() > MAX_BINARY_BYTES {
        return false;
    }
    let Ok(content) = fs::read(path) else {
        return false;
    };
    content
        .windows(MARKER.len())
        .any(|window| window == marker_slice())
}

fn marker_slice() -> &'static [u8] {
    MARKER
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fixture(version: &str, blocks: &[&[u8]]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("fixture");
        file.write_all(b"\x7fELF-NovaGPT-fixture\nplatform_handle\x00content\x00")
            .expect("header");
        file.write_all(version.as_bytes()).expect("version");
        file.write_all(b"\x00\n").expect("terminator");
        for block in blocks {
            let mut padded = block.to_vec();
            padded.resize(220, b' ');
            file.write_all(&padded).expect("block");
        }
        file.flush().expect("flush");
        file
    }

    #[test]
    fn binary_version_matches_exact_marker() {
        let file = fixture("156.0.8078.80", &[]);
        assert_eq!(
            binary_version(file.path()).expect("version"),
            ChromeVersion::new(156, 0, 8078, 80)
        );
    }

    #[test]
    fn patches_every_bounded_block_and_preserves_length() {
        let file = fixture(
            "156.0.8078.80",
            &[
                b"{window.cdc_do_not_use_1 = true;}",
                b"{window.cdc_do_not_use_2 = 1 + 2;}",
            ],
        );
        let before = fs::metadata(file.path()).expect("metadata").len();
        let report = patch_driver(file.path()).expect("patch");
        assert_eq!(report.status, PatchStatus::Patched);
        assert_eq!(report.blocks, 2);
        assert_eq!(fs::metadata(file.path()).expect("metadata").len(), before);
        let bytes = fs::read(file.path()).expect("patched");
        assert!(
            !bytes
                .windows(INJECTION_PREFIX.len())
                .any(|w| w == INJECTION_PREFIX)
        );
        assert!(bytes.windows(MARKER.len()).any(|w| w == MARKER));
        assert_eq!(
            binary_version(file.path()).expect("version retained"),
            ChromeVersion::new(156, 0, 8078, 80)
        );
        assert_eq!(
            patch_driver(file.path()).expect("idempotent"),
            PatchReport {
                status: PatchStatus::AlreadyPatched,
                blocks: 0
            }
        );
    }

    #[test]
    fn refuses_foreign_patch_and_unknown_layout_without_modifying_bytes() {
        let foreign = fixture("156.0.8078.80", &[b"{window.cdc_x = 1;}"]);
        let mut bytes = fs::read(foreign.path()).expect("read");
        bytes.extend_from_slice(b"undetected chromedriver");
        fs::write(foreign.path(), &bytes).expect("write");
        let before = fs::read(foreign.path()).expect("read");
        assert_eq!(
            patch_driver(foreign.path()),
            Err(UpstreamError::ForeignPatch)
        );
        assert_eq!(fs::read(foreign.path()).expect("read"), before);

        let plain = fixture("156.0.8078.80", &[]);
        let before = fs::read(plain.path()).expect("read");
        assert_eq!(patch_driver(plain.path()), Err(UpstreamError::Unpatchable));
        assert_eq!(fs::read(plain.path()).expect("read"), before);
    }

    #[test]
    fn refuses_truncated_or_oversized_injection_blocks() {
        let file = fixture("156.0.8078.80", &[b"{window.cdc_unterminated"]);
        let before = fs::read(file.path()).expect("read");
        assert_eq!(patch_driver(file.path()), Err(UpstreamError::Unpatchable));
        assert_eq!(fs::read(file.path()).expect("read"), before);
    }

    #[test]
    fn embedded_release_string_requires_a_component_boundary() {
        let version = ChromeVersion::new(156, 0, 8078, 80);
        assert!(contains_version(b"prefix 156.0.8078.80\0", &version));
        assert!(contains_version(b"156.0.8078.80", &version));
        assert!(!contains_version(b"156.0.8078.801", &version));
        assert!(!contains_version(b"156.0.8078.8", &version));
        assert!(!contains_version(b"1156.0.8078.80", &version));
    }

    #[test]
    fn version_scan_is_bounded_and_requires_four_components() {
        assert_eq!(find_version(b"prefix"), None);
        assert_eq!(
            find_version(b"platform_handle\x00content\x00156.0.1.2extra"),
            Some(ChromeVersion::new(156, 0, 1, 2))
        );
        assert_eq!(
            find_version(b"platform_handle\x00content\x00156.0.1extra"),
            None
        );
    }
}
