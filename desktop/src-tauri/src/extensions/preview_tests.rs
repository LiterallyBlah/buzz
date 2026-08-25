//! Tests for read-only package inspection.

use std::fs;
use std::io::Write;

use super::*;

fn manifest_json_text(id: &str) -> String {
    format!(r#"{{ "id": "{id}", "name": "Demo", "version": "0.1.0", "entry": "index.html" }}"#)
}

fn write_zip(entries: &[(&str, &[u8])]) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().expect("temp zip");
    let mut buffer = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buffer));
        let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
        for (name, body) in entries {
            writer.start_file(*name, options).expect("start_file");
            writer.write_all(body).expect("write");
        }
        writer.finish().expect("finish");
    }
    file.write_all(&buffer).expect("write zip");
    file.flush().expect("flush");
    file
}

#[test]
fn previews_a_directory_package_without_writing_anything() {
    let source = tempfile::tempdir().expect("tempdir");
    let body = manifest_json_text("demo");
    fs::write(source.path().join("extension.json"), &body).expect("manifest");
    fs::write(source.path().join("index.html"), b"<!doctype html>").expect("entry");

    let before = fs::read_dir(source.path()).unwrap().count();
    let preview = preview_package(source.path()).expect("preview");
    let after = fs::read_dir(source.path()).unwrap().count();

    assert_eq!(preview.manifest_json, body);
    assert_eq!(before, after, "inspection must not create anything");
}

#[test]
fn previews_a_zip_package_without_extracting_it() {
    // The archive is placed in a directory of this test's own, not the shared
    // system temp dir: counting entries in a directory other tests also write
    // to measures their concurrency, not this function's behaviour.
    let home = tempfile::tempdir().expect("tempdir");
    let body = manifest_json_text("demo");
    let staged = write_zip(&[
        ("extension.json", body.as_bytes()),
        ("index.html", b"<!doctype html>"),
    ]);
    let archive_path = home.path().join("package.zip");
    fs::copy(staged.path(), &archive_path).expect("place archive");

    let before = entry_count(home.path());
    let preview = preview_package(&archive_path).expect("preview");
    let after = entry_count(home.path());

    assert_eq!(preview.manifest_json, body);
    assert_eq!(before, after, "inspection must not extract");
}

/// Number of entries directly inside `dir`.
fn entry_count(dir: &std::path::Path) -> usize {
    fs::read_dir(dir).expect("read_dir").flatten().count()
}

#[test]
fn returns_malformed_json_verbatim_rather_than_judging_it() {
    // The UI's zod layer explains what is wrong with a manifest, so preview
    // must hand back exactly what the package ships — including nonsense.
    let source = tempfile::tempdir().expect("tempdir");
    let broken = r#"{ "id": "demo", "name": "#;
    fs::write(source.path().join("extension.json"), broken).expect("manifest");

    let preview = preview_package(source.path()).expect("preview should not validate");
    assert_eq!(preview.manifest_json, broken);
}

#[test]
fn a_manifest_requesting_a_non_signable_kind_still_previews() {
    // Semantics belong to the Rust loader at install time; preview is shape-
    // agnostic so the UI can show what was asked for before it is refused.
    let source = tempfile::tempdir().expect("tempdir");
    let body = r#"{ "id": "demo", "name": "Demo", "version": "1", "entry": "index.html",
                    "scopes": { "sign": [ { "kind": 30177, "channels": ["x"] } ] } }"#;
    fs::write(source.path().join("extension.json"), body).expect("manifest");

    assert!(preview_package(source.path()).is_ok());
}

#[test]
fn rejects_a_package_with_no_manifest_at_its_root() {
    let source = tempfile::tempdir().expect("tempdir");
    fs::write(source.path().join("index.html"), b"<!doctype html>").expect("entry");

    let Err(error) = preview_package(source.path()) else {
        panic!("a package with no manifest must not preview");
    };
    assert!(error.contains("no extension.json"), "got: {error}");
}

#[test]
fn does_not_reach_into_a_wrapper_directory_inside_a_zip() {
    // Preview must agree with the installer about where the root is, or the UI
    // would validate a manifest the installer will not read.
    let body = manifest_json_text("demo");
    let archive = write_zip(&[("my-ext/extension.json", body.as_bytes())]);

    assert!(
        preview_package(archive.path()).is_err(),
        "preview reached into a wrapper the installer rejects"
    );
}

#[test]
fn rejects_an_oversized_manifest_rather_than_truncating_it() {
    let source = tempfile::tempdir().expect("tempdir");
    let huge = "x".repeat((MAX_MANIFEST_BYTES as usize) + 1);
    fs::write(source.path().join("extension.json"), &huge).expect("manifest");

    let Err(error) = preview_package(source.path()) else {
        panic!("an oversized manifest must be rejected");
    };
    assert!(error.contains("larger than"), "got: {error}");
}

#[test]
fn preview_serializes_in_camel_case_for_the_ipc_contract() {
    let preview = ExtensionPackagePreview {
        source: "/tmp/demo".to_string(),
        manifest_json: "{}".to_string(),
    };
    let value = serde_json::to_value(&preview).expect("serialize");
    let object = value.as_object().expect("object");
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["manifestJson", "source"]);
}
