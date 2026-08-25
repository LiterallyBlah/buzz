//! Read-only inspection of a candidate extension package.
//!
//! The frontend cannot read arbitrary local paths, so decision 006's "zod
//! validates the manifest in the install UI" half needs a way to *see* a
//! manifest without installing it. This module is that seam: given a directory
//! or zip the user picked, return its `extension.json` bytes and nothing else.
//!
//! Two properties this must keep:
//!
//! 1. **It never writes.** No staging, no extraction to disk, no install side
//!    effects. Inspecting a hostile package must be as safe as not opening it.
//! 2. **It is not a validator.** It returns the manifest *as the package ships
//!    it*, including malformed JSON, so the UI can explain what is wrong.
//!    Authority still sits with the Rust loader at install time — this is a
//!    preview, and a package that previews cleanly can still be rejected.
//!
//! P5's grant-presentation UI reads the same preview to show what an extension
//! is asking for before the user agrees to it, which is why this returns the
//! whole manifest rather than a pre-digested summary.

use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::manifest::MANIFEST_FILE_NAME;

/// Largest `extension.json` this will read into memory.
///
/// A manifest is a small declaration; a multi-megabyte one is either a mistake
/// or an attempt to make the host allocate. The cap is applied to a zip entry
/// before decompressing it in full, so a lying header gains nothing.
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

/// A candidate package's manifest, as shipped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionPackagePreview {
    /// The directory or zip that was inspected.
    pub source: String,
    /// Raw `extension.json` contents. Not parsed and not validated here.
    pub manifest_json: String,
}

/// Read `extension.json` out of a package directory or zip without installing.
pub(crate) fn preview_package(source: &Path) -> Result<ExtensionPackagePreview, String> {
    let manifest_json = if source.is_dir() {
        read_manifest_from_directory(source)?
    } else {
        read_manifest_from_zip(source)?
    };
    Ok(ExtensionPackagePreview {
        source: source.to_string_lossy().into_owned(),
        manifest_json,
    })
}

fn read_manifest_from_directory(source: &Path) -> Result<String, String> {
    let path = source.join(MANIFEST_FILE_NAME);
    if !path.is_file() {
        return Err(format!(
            "{MANIFEST_FILE_NAME}: the package has no {MANIFEST_FILE_NAME} at its root"
        ));
    }
    let file = std::fs::File::open(&path)
        .map_err(|error| format!("{MANIFEST_FILE_NAME}: could not be read: {error}"))?;
    read_capped(file)
}

fn read_manifest_from_zip(source: &Path) -> Result<String, String> {
    let file = std::fs::File::open(source)
        .map_err(|error| format!("could not open the extension archive: {error}"))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| format!("could not read the extension archive: {error}"))?;

    // `by_name` resolves the literal root entry only — no traversal, and no
    // fallback to a nested copy, so this agrees with the installer's rule that
    // the manifest lives at the package root.
    let entry = archive.by_name(MANIFEST_FILE_NAME).map_err(|_| {
        format!("{MANIFEST_FILE_NAME}: the package has no {MANIFEST_FILE_NAME} at its root")
    })?;
    read_capped(entry)
}

/// Read at most [`MAX_MANIFEST_BYTES`], erroring rather than truncating.
///
/// Truncating would hand the UI a manifest that is not what the package
/// contains, and a "your JSON is malformed" message for a file that is fine.
fn read_capped<R: Read>(source: R) -> Result<String, String> {
    let mut buffer = Vec::new();
    source
        .take(MAX_MANIFEST_BYTES.saturating_add(1))
        .read_to_end(&mut buffer)
        .map_err(|error| format!("{MANIFEST_FILE_NAME}: could not be read: {error}"))?;
    if buffer.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(format!(
            "{MANIFEST_FILE_NAME}: larger than {} KiB",
            MAX_MANIFEST_BYTES / 1024
        ));
    }
    String::from_utf8(buffer).map_err(|_| format!("{MANIFEST_FILE_NAME}: is not valid UTF-8 text"))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "preview_tests.rs"]
mod preview_tests;
