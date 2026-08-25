//! Platform-neutral path rules for extension package contents.
//!
//! Every path that arrives from an extension package — a zip entry name, a
//! path relative to an install source directory, or the manifest's `entry`
//! field — is checked here before it is joined onto a host directory.
//!
//! The logic (and the reasoning below) is deliberately a copy of
//! `commands::agent_discovery::managed_node::validate_managed_node_zip_entries`
//! rather than a shared helper: the install path owns its own guard so a change
//! to one archive consumer cannot silently weaken the other.

/// Why a package path was rejected.
///
/// [`PackagePathError::describe`] is the user-facing fragment callers embed in
/// their own message, so the same rule reads naturally whether it rejected a
/// zip entry ("package contains path traversal: …") or a manifest field
/// ("`entry` … contains path traversal: …").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackagePathError {
    /// The path was the empty string.
    Empty,
    /// The path was rooted or drive/UNC-prefixed under some host's grammar.
    Absolute,
    /// The path had a `..` component under either separator.
    Traversal,
}

impl PackagePathError {
    /// The user-facing noun phrase for this rejection.
    pub(crate) fn describe(self) -> &'static str {
        match self {
            PackagePathError::Empty => "an empty path",
            PackagePathError::Absolute => "an absolute path",
            PackagePathError::Traversal => "path traversal",
        }
    }
}

/// Validate a package-relative path using platform-neutral string logic.
///
/// `std::path::Path` is intentionally avoided: its `is_absolute()` and
/// `Component` parsing use BUILD-HOST grammar, so `/etc/passwd` is not
/// `is_absolute()` on Windows (no drive prefix), causing the check to lie on
/// the platform this guard exists to protect. Instead we apply pure string
/// rules that produce identical results on every host:
///
/// - Unix-rooted: starts with `/`
/// - Windows-rooted: starts with `\`, has a drive prefix (`X:`), or is UNC
///   (`\\` / `//`)
/// - Traversal: any component that is `..` when split on EITHER `/` or `\`
///
/// A path that passes may still be rejected later — extraction re-checks each
/// entry through `zip`'s own `enclosed_name()`, and directory installs reject
/// symlinks — but nothing reaches a `join()` without passing here first.
pub(crate) fn check_package_relative_path(path: &str) -> Result<(), PackagePathError> {
    if path.is_empty() {
        return Err(PackagePathError::Empty);
    }

    // Absolute-path checks (platform-neutral).
    if path.starts_with('/') || path.starts_with('\\') {
        return Err(PackagePathError::Absolute);
    }
    // Drive prefix: one ASCII letter followed by ':'
    if path.len() >= 2 && path.as_bytes()[1] == b':' && path.as_bytes()[0].is_ascii_alphabetic() {
        return Err(PackagePathError::Absolute);
    }
    // UNC prefix (`//` / `\\`) is already caught by the `starts_with` checks
    // above; noted explicitly so a future edit does not "simplify" them away.

    // Traversal: split on both separators and check each component.
    if path.split(['/', '\\']).any(|component| component == "..") {
        return Err(PackagePathError::Traversal);
    }

    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "package_path_tests.rs"]
mod package_path_tests;
