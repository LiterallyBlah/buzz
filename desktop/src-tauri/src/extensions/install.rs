//! Installing an extension package into `<app-data>/extensions/<id>`.
//!
//! v1 distribution is a local directory or zip only, and an update is a manual
//! re-install (buzz-extensions decision 008). Nothing here touches the network.
//!
//! # Stage then swap
//!
//! Both sources extract/copy into a staging directory created *inside* the
//! extensions directory — same filesystem, so the final move is a rename — and
//! the manifest and entry file are validated **there**. Only then is the
//! destination replaced. A failure at any point leaves the destination exactly
//! as it was, and the staging directory is removed on every error path
//! (`tempfile::TempDir`'s drop covers everything before the swap; the swap
//! itself cleans up explicitly).

use std::fs;
use std::io::Read;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

#[cfg(test)]
use super::manifest::{load_and_validate_manifest, ExtensionManifest};
use super::package_path::check_package_relative_path;

/// Prefix for a staging directory. Dot-prefixed so
/// [`super::list_installed_in`] skips it — a half-built package is never an
/// installed one.
#[cfg(test)]
const STAGING_PREFIX: &str = ".staging-";

/// Prefix for the holder a replaced install is parked in during the swap.
const REPLACED_PREFIX: &str = ".replaced-";

/// Largest number of files + directories a package may contain.
///
/// A zip bomb does not need to be large on disk; it needs to be large once
/// expanded. 4096 entries is far above any plausible webview package (the
/// founding scenarios are a document, a script bundle and a stylesheet) and far
/// below a count that would exhaust inodes or stall the install. Checked
/// against the zip central directory before a single byte is written.
pub(crate) const MAX_PACKAGE_ENTRIES: usize = 4096;

/// Largest total uncompressed size a package may expand to.
///
/// 128 MiB is generous for a page-plus-assets bundle and bounded enough that a
/// deliberately over-compressed archive cannot fill the user's disk. For zips
/// the cap is charged against bytes **actually written**, not the sizes the
/// central directory declares, so a lying header gains nothing; each entry is
/// read through a `take()` of the remaining allowance so at most one byte past
/// the cap is ever produced.
pub(crate) const MAX_PACKAGE_UNCOMPRESSED_BYTES: u64 = 128 * 1024 * 1024;

/// Deepest directory nesting a package may contain.
///
/// Bounds the recursive directory walk so a pathological source tree cannot
/// exhaust the stack.
pub(crate) const MAX_PACKAGE_DEPTH: usize = 32;

/// Running allowance a package is charged against while it is staged.
struct Budget {
    entries_left: usize,
    bytes_left: u64,
}

impl Budget {
    fn new() -> Self {
        Self {
            entries_left: MAX_PACKAGE_ENTRIES,
            bytes_left: MAX_PACKAGE_UNCOMPRESSED_BYTES,
        }
    }

    fn take_entry(&mut self) -> Result<(), String> {
        self.entries_left = self.entries_left.checked_sub(1).ok_or_else(|| {
            format!("package has more than {MAX_PACKAGE_ENTRIES} files and folders")
        })?;
        Ok(())
    }

    fn take_bytes(&mut self, bytes: u64) -> Result<(), String> {
        self.bytes_left = self.bytes_left.checked_sub(bytes).ok_or_else(|| {
            format!(
                "package expands to more than {} MiB",
                MAX_PACKAGE_UNCOMPRESSED_BYTES / (1024 * 1024)
            )
        })?;
        Ok(())
    }
}

/// Stage a source directory, validate it, and swap it into `<base>/<id>`.
///
/// Returns the validated manifest and the directory it was installed to.
#[cfg(test)]
pub(crate) fn install_from_directory(
    base_dir: &Path,
    source_dir: &Path,
) -> Result<(ExtensionManifest, PathBuf), String> {
    install_staged(base_dir, |staging| stage_directory(staging, source_dir))
}

/// Stage a zip archive, validate it, and swap it into `<base>/<id>`.
///
/// Returns the validated manifest and the directory it was installed to.
#[cfg(test)]
pub(crate) fn install_from_zip(
    base_dir: &Path,
    archive_path: &Path,
) -> Result<(ExtensionManifest, PathBuf), String> {
    install_staged(base_dir, |staging| stage_zip(staging, archive_path))
}

/// The shared stage → validate → swap sequence.
#[cfg(test)]
fn install_staged<F>(base_dir: &Path, stage: F) -> Result<(ExtensionManifest, PathBuf), String>
where
    F: FnOnce(&Path) -> Result<(), String>,
{
    fs::create_dir_all(base_dir)
        .map_err(|error| format!("could not create the extensions folder: {error}"))?;

    let staging = tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .tempdir_in(base_dir)
        .map_err(|error| format!("could not create a staging folder: {error}"))?;

    // Every `?` from here until `staging.keep()` drops the TempDir, which
    // removes the partially staged tree.
    stage(staging.path())?;
    let manifest = load_and_validate_manifest(staging.path())?;

    let destination = base_dir.join(&manifest.id);
    let staged_path = staging.keep();
    if let Err(error) = swap_into_place(base_dir, &staged_path, &destination) {
        let _ = fs::remove_dir_all(&staged_path);
        return Err(error);
    }
    Ok((manifest, destination))
}

/// Move `staged` onto `destination`, replacing any existing install.
///
/// Re-installing over an existing id is expected, not an error (decision 008:
/// updates are a manual re-install). The previous tree is parked in a
/// dot-prefixed holder first so a failed rename can put it back rather than
/// leaving the user with nothing.
pub(super) fn swap_into_place(
    base_dir: &Path,
    staged: &Path,
    destination: &Path,
) -> Result<(), String> {
    let parked = if destination.exists() {
        let holder = tempfile::Builder::new()
            .prefix(REPLACED_PREFIX)
            .tempdir_in(base_dir)
            .map_err(|error| format!("could not prepare to replace the extension: {error}"))?;
        let slot = holder.path().join("previous");
        fs::rename(destination, &slot)
            .map_err(|error| format!("could not replace the installed extension: {error}"))?;
        Some((holder, slot))
    } else {
        None
    };

    match fs::rename(staged, destination) {
        // Dropping the holder removes the replaced tree.
        Ok(()) => Ok(()),
        Err(error) => match parked {
            Some((holder, slot)) => Err(restore_or_preserve(holder, &slot, destination, &error)),
            None => Err(format!("could not install the extension: {error}")),
        },
    }
}

/// Put the parked previous install back, or — if that also fails — keep it.
///
/// The failure that matters is the second one. Dropping the holder is what
/// removes the replaced tree, so ignoring a failed rollback and letting the
/// holder drop would delete the user's only remaining copy in exactly the
/// situation where they still need it. Instead the holder is kept and its
/// location is named in the error, so a rare filesystem fault costs the user a
/// manual move rather than their installed extension.
fn restore_or_preserve(
    holder: tempfile::TempDir,
    slot: &Path,
    destination: &Path,
    install_error: &std::io::Error,
) -> String {
    if fs::rename(slot, destination).is_ok() {
        // Restored; dropping the (now empty) holder is correct.
        return format!("could not install the extension: {install_error}");
    }
    let kept = holder.keep();
    format!(
        "could not install the extension: {install_error}. The previously installed version could not be restored automatically — its files have been preserved at {} and can be moved back to {}",
        kept.join("previous").display(),
        destination.display()
    )
}

// ── Directory source ─────────────────────────────────────────────────────────

/// Copy a source directory into `staging`, rejecting anything unsafe.
///
/// A symlink **anywhere** in the tree is rejected rather than skipped: a
/// symlink is a package escape (it names a path outside the package that the
/// installed copy would then read from or serve), and silently dropping it
/// would install a package that does not do what its author shipped.
pub(super) fn stage_directory(staging: &Path, source_dir: &Path) -> Result<(), String> {
    // The root itself is followed if the user picked a symlinked folder — that
    // was their own choice — but nothing beneath it is.
    let metadata = fs::metadata(source_dir)
        .map_err(|error| format!("could not read the selected folder: {error}"))?;
    if !metadata.is_dir() {
        return Err("the selected extension package is not a folder".to_string());
    }

    let mut budget = Budget::new();
    copy_tree(source_dir, staging, "", 0, &mut budget)
}

fn copy_tree(
    source: &Path,
    destination: &Path,
    relative: &str,
    depth: usize,
    budget: &mut Budget,
) -> Result<(), String> {
    if depth > MAX_PACKAGE_DEPTH {
        return Err(format!(
            "package nests folders more than {MAX_PACKAGE_DEPTH} levels deep"
        ));
    }

    let entries = fs::read_dir(source)
        .map_err(|error| format!("could not read the package folder: {error}"))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("could not read a package entry: {error}"))?;
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            return Err("package contains a file name that is not valid UTF-8".to_string());
        };

        let child_relative = if relative.is_empty() {
            name.to_string()
        } else {
            format!("{relative}/{name}")
        };
        if let Err(reason) = check_package_relative_path(&child_relative) {
            return Err(format!(
                "package contains {}: {child_relative}",
                reason.describe()
            ));
        }

        let source_child = entry.path();
        let metadata = fs::symlink_metadata(&source_child)
            .map_err(|error| format!("could not read {child_relative}: {error}"))?;
        if metadata.is_symlink() {
            return Err(format!("package contains a symlink: {child_relative}"));
        }

        budget.take_entry()?;
        let destination_child = destination.join(name);
        if metadata.is_dir() {
            fs::create_dir(&destination_child)
                .map_err(|error| format!("could not create {child_relative}: {error}"))?;
            copy_tree(
                &source_child,
                &destination_child,
                &child_relative,
                depth + 1,
                budget,
            )?;
        } else if metadata.is_file() {
            budget.take_bytes(metadata.len())?;
            fs::copy(&source_child, &destination_child)
                .map_err(|error| format!("could not copy {child_relative}: {error}"))?;
        } else {
            return Err(format!(
                "package contains an unsupported file type: {child_relative}"
            ));
        }
    }
    Ok(())
}

// ── Zip source ───────────────────────────────────────────────────────────────

/// Extract a zip archive into `staging`, rejecting anything unsafe.
pub(super) fn stage_zip(staging: &Path, archive_path: &Path) -> Result<(), String> {
    let file = fs::File::open(archive_path)
        .map_err(|error| format!("could not open the extension archive: {error}"))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| format!("could not read the extension archive: {error}"))?;
    validate_extension_zip_entries(&archive)?;
    extract_extension_zip(&mut archive, staging)
}

/// Validate ZIP entry names using platform-neutral string logic.
///
/// The per-entry rules — and the reason they are string rules rather than
/// `std::path` ones — live in [`check_package_relative_path`]. This pass runs
/// over the central directory *before* a single byte is written, so a hostile
/// archive is rejected without creating any files; extraction then re-checks
/// every entry through `zip`'s own `enclosed_name()`.
/// Path components of a zip entry name, ignoring empty ones.
///
/// Split on both separators for the same reason
/// [`check_package_relative_path`] does: a zip is written by some other host
/// and its own grammar must not decide what a component is here.
fn zip_entry_components(name: &str) -> Vec<&str> {
    name.split(['/', '\\'])
        .filter(|component| !component.is_empty())
        .collect()
}

/// Validate every zip entry against the central directory, before a byte is
/// written.
///
/// Three rules, all enforced here rather than during extraction so a hostile
/// archive is rejected before it can create anything:
///
/// 1. **Traversal** — the platform-neutral relative-path rules.
/// 2. **Depth** — an entry nested deeper than [`MAX_PACKAGE_DEPTH`] is
///    rejected. The directory installer bounds depth by recursion; the zip path
///    has no recursion to bound, so without this check the cap applied to one
///    source and not the other.
/// 3. **Entry count, including implicit parents** — extraction calls
///    `create_dir_all`, so one record `a/b/c/d.txt` materialises four paths. The
///    count that matters is the number of *distinct* paths the archive causes to
///    exist, not the number of records it declares; charging records alone let
///    an archive create far more directories than [`MAX_PACKAGE_ENTRIES`]
///    suggests.
fn validate_extension_zip_entries(archive: &zip::ZipArchive<fs::File>) -> Result<(), String> {
    // Distinct paths the archive will cause to exist: every record plus every
    // directory implied by one. A `BTreeSet` because the same parent is implied
    // by many records and must only be charged once.
    let mut distinct_paths: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for index in 0..archive.len() {
        let name = archive
            .name_for_index(index)
            .ok_or_else(|| format!("extension archive entry {index}: missing name"))?;
        if let Err(reason) = check_package_relative_path(name) {
            return Err(format!("package contains {}: {name}", reason.describe()));
        }

        let components = zip_entry_components(name);
        if components.is_empty() {
            continue;
        }

        // Depth is the number of ancestor directories, matching `copy_tree`,
        // where a child of the package root is handled at depth 0. A trailing
        // separator marks a directory record, whose own depth is its component
        // count rather than one less.
        let is_directory_record = name.ends_with('/') || name.ends_with('\\');
        let depth = if is_directory_record {
            components.len()
        } else {
            components.len() - 1
        };
        if depth > MAX_PACKAGE_DEPTH {
            return Err(format!(
                "package nests folders more than {MAX_PACKAGE_DEPTH} levels deep: {name}"
            ));
        }

        // Charge the record and every directory it implies.
        let mut prefix = String::new();
        for component in &components {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            distinct_paths.insert(prefix.clone());
            if distinct_paths.len() > MAX_PACKAGE_ENTRIES {
                return Err(format!(
                    "package has more than {MAX_PACKAGE_ENTRIES} files and folders"
                ));
            }
        }
    }

    Ok(())
}

/// Write every entry beneath `destination_dir`, enforcing the size budget.
fn extract_extension_zip(
    archive: &mut zip::ZipArchive<fs::File>,
    destination_dir: &Path,
) -> Result<(), String> {
    let mut budget = Budget::new();

    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| format!("extension archive entry {index}: {error}"))?;

        // A zip can encode a symlink in its unix mode bits. Writing one would
        // reintroduce the escape the directory installer refuses, so reject it
        // for the same reason.
        if entry.is_symlink() {
            return Err(format!("package contains a symlink: {}", entry.name()));
        }

        let outpath = match entry.enclosed_name() {
            Some(path) => destination_dir.join(path),
            None => return Err(format!("package contains an unsafe path: {}", entry.name())),
        };

        budget.take_entry()?;
        if entry.is_dir() {
            fs::create_dir_all(&outpath)
                .map_err(|error| format!("could not create a folder from the archive: {error}"))?;
            continue;
        }

        if let Some(parent) = outpath.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("could not create a folder from the archive: {error}"))?;
        }
        let mut out = fs::File::create(&outpath)
            .map_err(|error| format!("could not write a file from the archive: {error}"))?;

        // Read at most one byte past the remaining allowance: an over-large
        // entry is caught here rather than written out in full, and the charge
        // below is against bytes that really came out of the decompressor.
        let allowance = budget.bytes_left;
        let mut limited = (&mut entry).take(allowance.saturating_add(1));
        let written = std::io::copy(&mut limited, &mut out)
            .map_err(|error| format!("could not extract a file from the archive: {error}"))?;
        budget.take_bytes(written)?;
    }

    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "install_tests.rs"]
mod install_tests;
