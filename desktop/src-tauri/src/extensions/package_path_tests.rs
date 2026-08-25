//! Tests for the platform-neutral package-path rules.
//!
//! Kept in a sibling file so `package_path.rs` stays under the 1000-line gate;
//! `#[path]`-included from there.

use super::*;

#[test]
fn accepts_ordinary_relative_paths() {
    for path in [
        "index.html",
        "assets/app.js",
        "assets/nested/deep/style.css",
        "a.b.c",
        "..hidden",
        "dir/..name/file",
    ] {
        assert_eq!(
            check_package_relative_path(path),
            Ok(()),
            "expected {path:?} to be accepted"
        );
    }
}

#[test]
fn rejects_empty_path() {
    assert_eq!(
        check_package_relative_path(""),
        Err(PackagePathError::Empty)
    );
}

#[test]
fn rejects_unix_rooted_path() {
    assert_eq!(
        check_package_relative_path("/etc/passwd"),
        Err(PackagePathError::Absolute)
    );
}

#[test]
fn rejects_backslash_rooted_path() {
    assert_eq!(
        check_package_relative_path("\\Windows\\system32\\evil.dll"),
        Err(PackagePathError::Absolute)
    );
}

#[test]
fn rejects_drive_prefixed_path() {
    assert_eq!(
        check_package_relative_path("C:\\evil\\payload.exe"),
        Err(PackagePathError::Absolute)
    );
    assert_eq!(
        check_package_relative_path("z:relative-looking"),
        Err(PackagePathError::Absolute)
    );
}

#[test]
fn rejects_unc_paths_under_either_separator() {
    assert_eq!(
        check_package_relative_path("\\\\server\\share\\evil"),
        Err(PackagePathError::Absolute)
    );
    assert_eq!(
        check_package_relative_path("//server/share/evil"),
        Err(PackagePathError::Absolute)
    );
}

#[test]
fn rejects_traversal_under_either_separator() {
    assert_eq!(
        check_package_relative_path("../../evil.txt"),
        Err(PackagePathError::Traversal)
    );
    assert_eq!(
        check_package_relative_path("assets\\..\\..\\evil.txt"),
        Err(PackagePathError::Traversal)
    );
    assert_eq!(
        check_package_relative_path("assets/../../evil.txt"),
        Err(PackagePathError::Traversal)
    );
    assert_eq!(
        check_package_relative_path(".."),
        Err(PackagePathError::Traversal)
    );
}

#[test]
fn describe_names_the_rule_that_fired() {
    assert_eq!(PackagePathError::Empty.describe(), "an empty path");
    assert_eq!(PackagePathError::Absolute.describe(), "an absolute path");
    assert_eq!(PackagePathError::Traversal.describe(), "path traversal");
}
