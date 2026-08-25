//! Tests for staging, extraction safety, and the stage-then-swap install.
//!
//! Kept in a sibling file so `install.rs` stays under the 1000-line gate;
//! `#[path]`-included from there.

use std::io::Write;

use super::*;

const CHANNEL: &str = "11111111-2222-4333-8444-555555555555";

/// A manifest body for `id` with one sign and one read scope.
fn manifest_json(id: &str, entry: &str) -> String {
    format!(
        r#"{{
  "id": "{id}",
  "name": "Demo Extension",
  "version": "1.2.3",
  "entry": "{entry}",
  "scopes": {{
    "identity": true,
    "sign": [ {{ "kind": 9, "channels": ["{CHANNEL}"] }} ],
    "read": [ {{ "kinds": [9], "channels": ["{CHANNEL}"] }} ]
  }},
  "egress": ["https://example.com"]
}}"#
    )
}

/// Build a well-formed package directory and return its temp dir.
fn package_dir(id: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("extension.json"),
        manifest_json(id, "index.html"),
    )
    .expect("write manifest");
    std::fs::write(
        dir.path().join("index.html"),
        b"<!doctype html><title>demo</title>",
    )
    .expect("write entry");
    std::fs::create_dir(dir.path().join("assets")).expect("mkdir assets");
    std::fs::write(dir.path().join("assets").join("app.js"), b"console.log(1)")
        .expect("write asset");
    dir
}

/// Write a zip from `(name, contents)` pairs; a `None` body means a directory.
fn write_zip(entries: &[(&str, Option<&[u8]>)]) -> tempfile::NamedTempFile {
    let mut buffer: Vec<u8> = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buffer));
        let options = zip::write::SimpleFileOptions::default();
        for (name, body) in entries {
            match body {
                Some(bytes) => {
                    writer.start_file(*name, options).expect("start_file");
                    writer.write_all(bytes).expect("write entry body");
                }
                None => {
                    writer.add_directory(*name, options).expect("add_directory");
                }
            }
        }
        writer.finish().expect("finish zip");
    }
    let mut file = tempfile::NamedTempFile::new().expect("named temp file");
    file.write_all(&buffer).expect("write zip to disk");
    file.flush().expect("flush zip");
    file
}

/// A zip holding a well-formed package.
fn benign_zip(id: &str) -> tempfile::NamedTempFile {
    let manifest = manifest_json(id, "index.html");
    write_zip(&[
        ("extension.json", Some(manifest.as_bytes())),
        ("index.html", Some(b"<!doctype html>")),
        ("assets/", None),
        ("assets/app.js", Some(b"console.log(1)")),
    ])
}

/// Every name directly under `base`, sorted.
fn entries_in(base: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(base)
        .expect("read base dir")
        .map(|entry| {
            entry
                .expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

// ── Zip extraction safety ────────────────────────────────────────────────────

#[test]
fn rejects_zip_slip_entries() {
    let manifest = manifest_json("demo", "index.html");
    let hostile: [(&str, &str); 4] = [
        ("../../evil.txt", "path traversal"),
        ("/etc/passwd", "an absolute path"),
        ("\\Windows\\system32\\evil.dll", "an absolute path"),
        ("C:\\evil\\payload.exe", "an absolute path"),
    ];

    for (name, needle) in hostile {
        let base = tempfile::tempdir().expect("tempdir");
        let archive = write_zip(&[
            ("extension.json", Some(manifest.as_bytes())),
            ("index.html", Some(b"<!doctype html>")),
            (name, Some(b"pwned")),
        ]);

        let Err(error) = install_from_zip(base.path(), archive.path()) else {
            panic!("a zip containing {name:?} must be rejected");
        };
        assert!(
            error.contains(needle) && error.contains(name),
            "expected {needle:?} and {name:?} in: {error}"
        );
        // Nothing was written: not the destination, not a staging leftover.
        assert!(
            entries_in(base.path()).is_empty(),
            "a rejected zip left {:?} behind",
            entries_in(base.path())
        );
    }
}

#[test]
fn rejects_a_zip_slip_entry_even_when_it_is_the_only_entry() {
    let base = tempfile::tempdir().expect("tempdir");
    let archive = write_zip(&[("../../../etc/passwd", Some(b"pwned"))]);

    let Err(error) = install_from_zip(base.path(), archive.path()) else {
        panic!("a traversal-only zip must be rejected");
    };
    assert!(error.contains("path traversal"), "got: {error}");
    assert!(entries_in(base.path()).is_empty());
}

/// The escape itself, not just the message.
///
/// The extensions folder is nested inside an outer temp dir so a `../../`
/// entry has somewhere real to land. The escape is asserted **before** the
/// error is inspected: with the traversal rules removed, this is the assertion
/// that fires, and it fires because a file appeared outside the folder Buzz
/// owns — not because a string changed.
#[test]
fn a_zip_slip_entry_never_lands_outside_the_extensions_folder() {
    let outer = tempfile::tempdir().expect("tempdir");
    let base = outer.path().join("extensions");
    std::fs::create_dir_all(&base).expect("mkdir base");
    let manifest = manifest_json("demo", "index.html");
    let archive = write_zip(&[
        ("extension.json", Some(manifest.as_bytes())),
        ("index.html", Some(b"<!doctype html>")),
        ("../../escaped.txt", Some(b"pwned")),
    ]);

    let result = install_from_zip(&base, archive.path());

    assert!(
        !outer.path().join("escaped.txt").exists(),
        "the zip-slip entry escaped the extensions folder"
    );
    let Err(error) = result else {
        panic!("a zip-slip entry must be rejected");
    };
    assert!(error.contains("path traversal"), "got: {error}");
    assert!(entries_in(&base).is_empty());
}

#[test]
fn benign_zip_installs_and_lands_only_under_the_destination() {
    let base = tempfile::tempdir().expect("tempdir");
    let archive = benign_zip("demo");

    let (manifest, path) =
        install_from_zip(base.path(), archive.path()).expect("benign zip should install");

    assert_eq!(manifest.id, "demo");
    assert_eq!(manifest.entry, "index.html");
    assert_eq!(path, base.path().join("demo"));
    assert_eq!(entries_in(base.path()), vec!["demo".to_string()]);
    assert!(path.join("extension.json").is_file());
    assert!(path.join("index.html").is_file());
    assert_eq!(
        std::fs::read(path.join("assets").join("app.js")).expect("read asset"),
        b"console.log(1)".to_vec()
    );
    assert_eq!(
        entries_in(&path),
        vec!["assets", "extension.json", "index.html"]
    );
}

#[test]
fn rejects_a_zip_whose_entry_count_exceeds_the_cap() {
    let base = tempfile::tempdir().expect("tempdir");
    let names: Vec<String> = (0..=MAX_PACKAGE_ENTRIES)
        .map(|index| format!("file-{index}.txt"))
        .collect();
    let entries: Vec<(&str, Option<&[u8]>)> = names
        .iter()
        .map(|name| (name.as_str(), Some(&b"x"[..])))
        .collect();
    let archive = write_zip(&entries);

    let Err(error) = install_from_zip(base.path(), archive.path()) else {
        panic!("an over-count zip must be rejected");
    };
    assert!(
        error.contains("more than 4096 files and folders"),
        "got: {error}"
    );
    assert!(entries_in(base.path()).is_empty());
}

#[test]
fn rejects_a_zip_that_expands_past_the_size_cap() {
    let base = tempfile::tempdir().expect("tempdir");
    // A highly compressible payload one byte past the cap: small on disk,
    // over-large once expanded — the shape a zip bomb takes.
    let manifest = manifest_json("demo", "index.html");
    let payload = vec![0u8; (MAX_PACKAGE_UNCOMPRESSED_BYTES + 1) as usize];
    let archive = write_zip(&[
        ("extension.json", Some(manifest.as_bytes())),
        ("index.html", Some(b"<!doctype html>")),
        ("bomb.bin", Some(&payload)),
    ]);

    let Err(error) = install_from_zip(base.path(), archive.path()) else {
        panic!("an over-size zip must be rejected");
    };
    assert!(
        error.contains("expands to more than 128 MiB"),
        "got: {error}"
    );
    assert!(entries_in(base.path()).is_empty());
}

// ── Directory install ────────────────────────────────────────────────────────

#[test]
fn installs_a_directory_package() {
    let base = tempfile::tempdir().expect("tempdir");
    let source = package_dir("demo");

    let (manifest, path) =
        install_from_directory(base.path(), source.path()).expect("directory install");

    assert_eq!(manifest.id, "demo");
    assert_eq!(manifest.version, "1.2.3");
    assert_eq!(manifest.egress, vec!["https://example.com".to_string()]);
    assert_eq!(path, base.path().join("demo"));
    assert_eq!(entries_in(base.path()), vec!["demo".to_string()]);
    assert!(path.join("index.html").is_file());
    assert!(path.join("assets").join("app.js").is_file());
}

#[cfg(unix)]
#[test]
fn rejects_a_symlink_at_the_top_of_the_source_tree() {
    let base = tempfile::tempdir().expect("tempdir");
    let source = package_dir("demo");
    std::os::unix::fs::symlink("/etc/passwd", source.path().join("secrets.txt"))
        .expect("create symlink");

    let Err(error) = install_from_directory(base.path(), source.path()) else {
        panic!("a symlink in the source tree must be rejected");
    };
    assert!(
        error.contains("package contains a symlink") && error.contains("secrets.txt"),
        "got: {error}"
    );
    assert!(entries_in(base.path()).is_empty());
}

#[cfg(unix)]
#[test]
fn rejects_a_symlink_nested_in_the_source_tree() {
    let base = tempfile::tempdir().expect("tempdir");
    let source = package_dir("demo");
    std::os::unix::fs::symlink("/etc", source.path().join("assets").join("escape"))
        .expect("create symlink");

    let Err(error) = install_from_directory(base.path(), source.path()) else {
        panic!("a nested symlink must be rejected");
    };
    assert!(
        error.contains("package contains a symlink") && error.contains("assets/escape"),
        "got: {error}"
    );
    assert!(entries_in(base.path()).is_empty());
}

#[test]
fn rejects_a_source_that_is_not_a_directory() {
    let base = tempfile::tempdir().expect("tempdir");
    let file = tempfile::NamedTempFile::new().expect("temp file");

    let Err(error) = install_from_directory(base.path(), file.path()) else {
        panic!("a file source must be rejected");
    };
    assert!(error.contains("is not a folder"), "got: {error}");
    assert!(entries_in(base.path()).is_empty());
}

// ── Stage then swap ──────────────────────────────────────────────────────────

#[test]
fn a_failed_install_leaves_no_destination_and_no_staging_leftovers() {
    let base = tempfile::tempdir().expect("tempdir");

    // Stages cleanly, then fails validation: the id is a traversal attempt.
    let source = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        source.path().join("extension.json"),
        manifest_json("../evil", "index.html"),
    )
    .expect("write manifest");
    std::fs::write(source.path().join("index.html"), b"<!doctype html>").expect("write entry");

    let Err(error) = install_from_directory(base.path(), source.path()) else {
        panic!("a traversal id must be rejected");
    };
    assert!(
        error.starts_with("extension id \"../evil\" is not valid"),
        "got: {error}"
    );
    assert!(
        entries_in(base.path()).is_empty(),
        "a failed install left {:?} behind",
        entries_in(base.path())
    );
    // `../evil` would have landed beside the extensions folder, not inside it.
    let sibling = base
        .path()
        .parent()
        .expect("the temp dir has a parent")
        .join("evil");
    assert!(
        !sibling.exists(),
        "a traversal id escaped the extensions folder"
    );
}

#[test]
fn a_missing_entry_file_fails_before_anything_is_installed() {
    let base = tempfile::tempdir().expect("tempdir");
    let source = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        source.path().join("extension.json"),
        manifest_json("demo", "index.html"),
    )
    .expect("write manifest");

    let Err(error) = install_from_directory(base.path(), source.path()) else {
        panic!("a package with no entry file must be rejected");
    };
    assert!(
        error.contains("is missing from the package"),
        "got: {error}"
    );
    assert!(entries_in(base.path()).is_empty());
}

#[test]
fn reinstalling_replaces_the_previous_package() {
    let base = tempfile::tempdir().expect("tempdir");

    let first = package_dir("demo");
    std::fs::write(first.path().join("only-in-first.txt"), b"v1").expect("write marker");
    install_from_directory(base.path(), first.path()).expect("first install");
    assert!(base.path().join("demo").join("only-in-first.txt").is_file());

    let second = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        second.path().join("extension.json"),
        manifest_json("demo", "index.html").replace("1.2.3", "2.0.0"),
    )
    .expect("write manifest");
    std::fs::write(second.path().join("index.html"), b"v2").expect("write entry");

    let (manifest, path) =
        install_from_directory(base.path(), second.path()).expect("re-install replaces");

    assert_eq!(manifest.version, "2.0.0");
    assert_eq!(entries_in(base.path()), vec!["demo".to_string()]);
    assert!(
        !path.join("only-in-first.txt").exists(),
        "re-install must replace, not merge"
    );
    assert_eq!(std::fs::read(path.join("index.html")).expect("read"), b"v2");
}

#[test]
fn a_failed_reinstall_leaves_the_existing_package_intact() {
    let base = tempfile::tempdir().expect("tempdir");

    let first = package_dir("demo");
    install_from_directory(base.path(), first.path()).expect("first install");

    // Second attempt fails validation after staging.
    let second = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        second.path().join("extension.json"),
        manifest_json("demo", "index.html").replace("\"kind\": 9", "\"kind\": 30177"),
    )
    .expect("write manifest");
    std::fs::write(second.path().join("index.html"), b"v2").expect("write entry");

    let Err(error) = install_from_directory(base.path(), second.path()) else {
        panic!("a non-grantable sign kind must be rejected");
    };
    assert!(
        error.contains("scopes.sign requests kind 30177"),
        "got: {error}"
    );
    assert_eq!(entries_in(base.path()), vec!["demo".to_string()]);
    assert_eq!(
        std::fs::read(base.path().join("demo").join("index.html")).expect("read"),
        b"<!doctype html><title>demo</title>".to_vec()
    );
}

#[test]
fn install_creates_the_extensions_folder_when_absent() {
    let parent = tempfile::tempdir().expect("tempdir");
    let base = parent.path().join("extensions");
    assert!(!base.exists());

    let source = package_dir("demo");
    let (_, path) = install_from_directory(&base, source.path()).expect("install");
    assert_eq!(path, base.join("demo"));
    assert!(base.is_dir());
}
