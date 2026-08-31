//! Tests for the installed-extension inventory and the IPC wire shape.
//!
//! Kept in a sibling file so `mod.rs` stays under the 1000-line gate;
//! `#[path]`-included from there.

use super::manifest::{ReadScope, SignScope};
use super::*;

const CHANNEL: &str = "11111111-2222-4333-8444-555555555555";

fn manifest_json(id: &str) -> String {
    format!(
        r#"{{
  "id": "{id}",
  "name": "Demo Extension",
  "version": "1.2.3",
  "entry": "index.html",
  "scopes": {{
    "identity": true,
    "storage": false,
    "extensionData": true,
    "sign": [ {{ "kind": 9, "channels": ["{CHANNEL}"] }} ],
    "read": [ {{ "kinds": [9, 45001], "channels": ["{CHANNEL}"] }} ]
  }},
  "egress": ["https://example.com"]
}}"#
    )
}

/// Write a package directory straight into `base` (bypassing install), so the
/// listing path can be exercised against hand-built on-disk states.
fn write_package(base: &Path, directory: &str, manifest: &str, entry: Option<&[u8]>) {
    let dir = base.join(directory);
    std::fs::create_dir_all(&dir).expect("mkdir package");
    std::fs::write(dir.join("extension.json"), manifest).expect("write manifest");
    if let Some(bytes) = entry {
        std::fs::write(dir.join("index.html"), bytes).expect("write entry");
    }
}

// ── Listing ──────────────────────────────────────────────────────────────────

#[test]
fn lists_nothing_when_the_folder_does_not_exist() {
    let parent = tempfile::tempdir().expect("tempdir");
    let installed = list_installed_in(&parent.path().join("extensions")).expect("list");
    assert!(installed.is_empty());
}

#[test]
fn lists_installed_packages_sorted_by_id() {
    let base = tempfile::tempdir().expect("tempdir");
    for id in ["zeta", "alpha", "middle"] {
        write_package(
            base.path(),
            id,
            &manifest_json(id),
            Some(b"<!doctype html>"),
        );
    }

    let installed = list_installed_in(base.path()).expect("list");
    let ids: Vec<&str> = installed.iter().map(|item| item.id.as_str()).collect();
    assert_eq!(ids, vec!["alpha", "middle", "zeta"]);

    let alpha = &installed[0];
    assert_eq!(alpha.name, "Demo Extension");
    assert_eq!(alpha.version, "1.2.3");
    assert_eq!(alpha.entry, "index.html");
    assert_eq!(alpha.path, base.path().join("alpha").to_string_lossy());
    assert!(alpha.scopes.identity);
    assert!(!alpha.scopes.storage);
    assert!(alpha.scopes.extension_data);
    assert_eq!(alpha.scopes.sign.len(), 1);
    assert_eq!(alpha.scopes.read[0].kinds, vec![9, 45001]);
    assert_eq!(alpha.egress, vec!["https://example.com".to_string()]);
    assert!(alpha.installed_at > 0, "installedAt should be a real time");
}

#[test]
fn skips_dot_prefixed_directories() {
    let base = tempfile::tempdir().expect("tempdir");
    write_package(base.path(), "demo", &manifest_json("demo"), Some(b"x"));
    // A staging leftover from an interrupted install must never be listed.
    write_package(
        base.path(),
        ".staging-abc123",
        &manifest_json("demo"),
        Some(b"x"),
    );

    let installed = list_installed_in(base.path()).expect("list");
    assert_eq!(installed.len(), 1);
    assert_eq!(installed[0].id, "demo");
}

#[test]
fn skips_a_package_whose_manifest_id_does_not_match_its_folder() {
    let base = tempfile::tempdir().expect("tempdir");
    write_package(
        base.path(),
        "demo",
        &manifest_json("someone-else"),
        Some(b"x"),
    );

    let installed = list_installed_in(base.path()).expect("list");
    assert!(installed.is_empty(), "got: {installed:?}");
}

#[test]
fn skips_a_package_with_an_invalid_manifest_without_failing_the_list() {
    let base = tempfile::tempdir().expect("tempdir");
    write_package(base.path(), "good", &manifest_json("good"), Some(b"x"));
    write_package(base.path(), "broken", "{ not json", Some(b"x"));
    write_package(
        base.path(),
        "no-entry-file",
        &manifest_json("no-entry-file"),
        None,
    );
    std::fs::create_dir(base.path().join("empty")).expect("mkdir");
    std::fs::write(base.path().join("stray.txt"), b"not a package").expect("write file");

    let installed = list_installed_in(base.path()).expect("list must not fail");
    let ids: Vec<&str> = installed.iter().map(|item| item.id.as_str()).collect();
    assert_eq!(ids, vec!["good"]);
}

#[cfg(unix)]
#[test]
fn skips_a_symlinked_package_directory() {
    let base = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("tempdir");
    write_package(outside.path(), "demo", &manifest_json("demo"), Some(b"x"));
    std::os::unix::fs::symlink(outside.path().join("demo"), base.path().join("demo"))
        .expect("symlink");

    let installed = list_installed_in(base.path()).expect("list");
    assert!(installed.is_empty(), "got: {installed:?}");
}

#[test]
fn an_installed_package_shows_up_in_the_listing() {
    let base = tempfile::tempdir().expect("tempdir");
    let source = tempfile::tempdir().expect("tempdir");
    std::fs::write(source.path().join("extension.json"), manifest_json("demo"))
        .expect("write manifest");
    std::fs::write(source.path().join("index.html"), b"<!doctype html>").expect("write entry");

    let installed = install_directory_in(base.path(), source.path()).expect("install");
    let listed = list_installed_in(base.path()).expect("list");

    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, installed.id);
    assert_eq!(listed[0].path, installed.path);
    assert_eq!(listed[0].scopes, installed.scopes);
    assert_eq!(listed[0].egress, installed.egress);
}

// ── IPC wire shape ───────────────────────────────────────────────────────────

#[test]
fn installed_extension_serializes_in_camel_case() {
    let extension = InstalledExtension {
        id: "demo".to_string(),
        name: "Demo".to_string(),
        version: "1.0.0".to_string(),
        entry: "index.html".to_string(),
        path: "/tmp/extensions/demo".to_string(),
        installed_at: 1_700_000_000,
        scopes: ExtensionScopes {
            identity: true,
            storage: false,
            extension_data: true,
            sign: vec![SignScope {
                kind: 9,
                channels: vec![CHANNEL.to_string()],
            }],
            read: vec![ReadScope {
                kinds: vec![9],
                channels: vec![CHANNEL.to_string()],
            }],
        },
        egress: vec!["https://example.com".to_string()],
        digest: "ab".repeat(32),
        enabled: false,
        granted: grants::GrantSelection::default(),
    };

    let value = serde_json::to_value(&extension).expect("serialize");
    let object = value.as_object().expect("object");
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "egress",
            "digest",
            "enabled",
            "entry",
            "granted",
            "id",
            "installedAt",
            "name",
            "path",
            "scopes",
            "version",
        ]
    );
    assert_eq!(object["installedAt"], serde_json::json!(1_700_000_000u64));

    let scopes = object["scopes"].as_object().expect("scopes object");
    let mut scope_keys: Vec<&str> = scopes.keys().map(String::as_str).collect();
    scope_keys.sort();
    assert_eq!(
        scope_keys,
        vec!["extensionData", "identity", "read", "sign", "storage"]
    );
    assert_eq!(scopes["sign"][0]["kind"], serde_json::json!(9));
    assert_eq!(scopes["read"][0]["kinds"], serde_json::json!([9]));
}

// ── Frame target resolution (P3) ─────────────────────────────────────────────

#[test]
fn a_frame_id_is_grammar_checked_before_it_is_joined_to_a_path() {
    // The id arrives from the webview. Joining it first and validating the
    // manifest afterwards would mean the traversing read already happened.
    let base = tempfile::tempdir().expect("tempdir");
    // Something loadable one level above the extensions dir, so a traversal
    // would have a real target rather than failing for want of a file.
    let outside = base.path().join("outside");
    std::fs::create_dir_all(&outside).expect("outside");
    std::fs::write(
        outside.join("extension.json"),
        br#"{ "id": "outside", "name": "Outside", "version": "1", "entry": "index.html" }"#,
    )
    .expect("manifest");
    std::fs::write(outside.join("index.html"), b"<!doctype html>").expect("entry");

    let extensions = base.path().join("extensions");
    std::fs::create_dir_all(&extensions).expect("extensions dir");

    for id in ["../outside", "..", "../../etc", "a/b", "Evil", ""] {
        let Err(error) = resolve_frame_manifest(&extensions, id) else {
            panic!("id {id:?} must not resolve to a frame target");
        };
        assert!(
            error.contains("is not valid"),
            "id {id:?} should fail the grammar, got: {error}"
        );
    }
    // The outside package really is loadable when addressed directly, so the
    // rejections above are the grammar working, not a missing fixture.
    resolve_frame_manifest(base.path(), "outside").expect("fixture should load directly");
}

#[test]
fn a_frame_target_resolves_for_an_installed_package() {
    let base = tempfile::tempdir().expect("tempdir");
    let root = base.path().join("demo");
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(
        root.join("extension.json"),
        br#"{ "id": "demo", "name": "Demo", "version": "1", "entry": "index.html" }"#,
    )
    .expect("manifest");
    std::fs::write(root.join("index.html"), b"<!doctype html>").expect("entry");

    let manifest = resolve_frame_manifest(base.path(), "demo").expect("should resolve");
    assert_eq!(manifest.id, "demo");
    assert_eq!(manifest.entry, "index.html");
}

#[test]
fn a_package_may_not_claim_an_id_other_than_its_folder() {
    let base = tempfile::tempdir().expect("tempdir");
    let root = base.path().join("demo");
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(
        root.join("extension.json"),
        br#"{ "id": "other", "name": "Other", "version": "1", "entry": "index.html" }"#,
    )
    .expect("manifest");
    std::fs::write(root.join("index.html"), b"<!doctype html>").expect("entry");

    let Err(error) = resolve_frame_manifest(base.path(), "demo") else {
        panic!("a mismatched manifest id must be refused");
    };
    assert!(error.contains("claiming id"), "got: {error}");
}
