//! Tests for `extension.json` parsing and validation.
//!
//! Kept in a sibling file so `manifest.rs` stays under the 1000-line gate;
//! `#[path]`-included from there.

use super::*;

/// Canonical lowercase hyphenated channel UUIDs. Both carry hex *letters* on
/// purpose: a digits-only UUID is unchanged by `to_uppercase`, which would make
/// the casing test below pass without the rule being there at all.
const CHANNEL_A: &str = "1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d";
const CHANNEL_B: &str = "66666666-7777-4888-8999-aaaaaaaaaaaa";

/// The `docs/BRIDGE_SPEC.md` §7 example manifest, which must always validate.
fn valid_manifest_json() -> String {
    format!(
        r#"{{
  "id": "equation-explorer",
  "name": "Equation Explorer",
  "version": "0.1.0",
  "entry": "index.html",
  "scopes": {{
    "identity": true,
    "storage": true,
    "extensionData": true,
    "sign": [ {{ "kind": 9, "channels": ["{CHANNEL_A}"] }} ],
    "read": [ {{ "kinds": [9, 45001], "channels": ["{CHANNEL_A}", "{CHANNEL_B}"] }} ]
  }},
  "egress": []
}}"#
    )
}

fn parse_and_validate(json: &str) -> Result<ExtensionManifest, String> {
    let manifest = parse_manifest(json.as_bytes())?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

/// The four required fields, as `(name, json value)`.
const REQUIRED_FIELDS: [(&str, &str); 4] = [
    ("id", "\"demo\""),
    ("name", "\"Demo\""),
    ("version", "\"1.0.0\""),
    ("entry", "\"index.html\""),
];

/// A minimal manifest with `field` replaced by a raw JSON `value`.
fn manifest_with(field: &str, value: &str) -> String {
    let body = REQUIRED_FIELDS
        .iter()
        .map(|(key, default)| {
            let rendered = if *key == field { value } else { default };
            format!("  \"{key}\": {rendered}")
        })
        .collect::<Vec<_>>()
        .join(",\n");
    format!("{{\n{body}\n}}")
}

/// A minimal manifest with `field` omitted entirely.
fn manifest_without(field: &str) -> String {
    let body = REQUIRED_FIELDS
        .iter()
        .filter(|(key, _)| *key != field)
        .map(|(key, value)| format!("  \"{key}\": {value}"))
        .collect::<Vec<_>>()
        .join(",\n");
    format!("{{\n{body}\n}}")
}

/// A minimal manifest carrying the supplied `scopes` object.
fn manifest_with_scopes(scopes: &str) -> String {
    format!(
        r#"{{
  "id": "demo",
  "name": "Demo",
  "version": "1.0.0",
  "entry": "index.html",
  "scopes": {scopes}
}}"#
    )
}

/// A minimal manifest carrying the supplied `egress` array.
fn manifest_with_egress(origin: &str) -> String {
    format!(
        r#"{{
  "id": "demo",
  "name": "Demo",
  "version": "1.0.0",
  "entry": "index.html",
  "egress": ["{origin}"]
}}"#
    )
}

// ── Shape ────────────────────────────────────────────────────────────────────

#[test]
fn accepts_the_spec_example_manifest() {
    let manifest = parse_and_validate(&valid_manifest_json()).expect("manifest should validate");
    assert_eq!(manifest.id, "equation-explorer");
    assert_eq!(manifest.name, "Equation Explorer");
    assert_eq!(manifest.version, "0.1.0");
    assert_eq!(manifest.entry, "index.html");
    assert!(manifest.scopes.identity);
    assert!(manifest.scopes.storage);
    assert!(manifest.scopes.extension_data);
    assert_eq!(manifest.scopes.sign.len(), 1);
    assert_eq!(manifest.scopes.sign[0].kind, 9);
    assert_eq!(
        manifest.scopes.sign[0].channels,
        vec![CHANNEL_A.to_string()]
    );
    assert_eq!(manifest.scopes.read[0].kinds, vec![9, 45001]);
    assert!(manifest.egress.is_empty());
}

#[test]
fn scopes_and_egress_default_to_nothing() {
    let json = r#"{ "id": "demo", "name": "Demo", "version": "1.0.0", "entry": "index.html" }"#;
    let manifest = parse_and_validate(json).expect("a bare manifest should validate");
    assert_eq!(manifest.scopes, ExtensionScopes::default());
    assert!(!manifest.scopes.identity);
    assert!(!manifest.scopes.storage);
    assert!(!manifest.scopes.extension_data);
    assert!(manifest.scopes.sign.is_empty());
    assert!(manifest.scopes.read.is_empty());
    assert!(manifest.egress.is_empty());
}

#[test]
fn rejects_unknown_top_level_field() {
    let json =
        r#"{ "id": "demo", "name": "Demo", "version": "1.0.0", "entry": "index.html", "foo": 1 }"#;
    let Err(error) = parse_and_validate(json) else {
        panic!("an unknown top-level field must be rejected");
    };
    assert!(
        error.starts_with("extension.json: ")
            && error.contains("unknown field")
            && error.contains("foo"),
        "expected an unknown-field error naming `foo`, got: {error}"
    );
}

#[test]
fn rejects_unknown_nested_fields() {
    let sign =
        format!(r#"{{ "sign": [ {{ "kind": 9, "channels": ["{CHANNEL_A}"], "nope": 1 }} ] }}"#);
    let read =
        format!(r#"{{ "read": [ {{ "kinds": [9], "channels": ["{CHANNEL_A}"], "nope": 1 }} ] }}"#);
    for scopes in [r#"{ "identity": true, "nope": true }"#, &sign, &read] {
        let Err(error) = parse_and_validate(&manifest_with_scopes(scopes)) else {
            panic!("an unknown nested field must be rejected in {scopes}");
        };
        assert!(
            error.contains("unknown field") && error.contains("nope"),
            "expected an unknown-field error naming `nope`, got: {error}"
        );
    }
}

#[test]
fn rejects_each_missing_required_field_by_name() {
    for (field, _) in REQUIRED_FIELDS {
        let Err(error) = parse_and_validate(&manifest_without(field)) else {
            panic!("a manifest missing `{field}` must be rejected");
        };
        assert!(
            error.contains("missing field") && error.contains(field),
            "expected a missing-field error naming `{field}`, got: {error}"
        );
    }
}

#[test]
fn rejects_empty_name_and_version() {
    let Err(error) = parse_and_validate(&manifest_with("name", "\"\"")) else {
        panic!("an empty name must be rejected");
    };
    assert!(error.contains("\"name\""), "got: {error}");

    let Err(error) = parse_and_validate(&manifest_with("version", "\"  \"")) else {
        panic!("a blank version must be rejected");
    };
    assert!(error.contains("\"version\""), "got: {error}");
}

// ── Id grammar ───────────────────────────────────────────────────────────────

#[test]
fn id_grammar_table() {
    let accepted = [
        "a",
        "0",
        "_",
        "demo",
        "equation-explorer",
        "equation_explorer",
        "ext-1_2-3",
        "_leading-underscore",
        "9lives",
    ];
    let rejected = [
        "../evil",
        "..",
        ".",
        "Evil",
        "-lead",
        "",
        "a/b",
        "a\\b",
        "a b",
        "a.b",
        "a:b",
        "évil",
        "demo/",
        "/demo",
        "..\\..\\evil",
        "con:",
    ];

    for id in accepted {
        assert!(is_valid_extension_id(id), "expected {id:?} to be accepted");
    }
    for id in rejected {
        assert!(!is_valid_extension_id(id), "expected {id:?} to be rejected");
    }
    // Length cap: at the limit is fine, one over is not.
    assert!(is_valid_extension_id(&"a".repeat(MAX_EXTENSION_ID_LEN)));
    assert!(!is_valid_extension_id(
        &"a".repeat(MAX_EXTENSION_ID_LEN + 1)
    ));
}

#[test]
fn traversal_id_is_rejected_by_the_manifest_validator() {
    let Err(error) = parse_and_validate(&manifest_with("id", "\"../evil\"")) else {
        panic!("a traversal id must be rejected");
    };
    assert!(
        error.starts_with("extension id \"../evil\" is not valid"),
        "got: {error}"
    );
}

// ── Sign scopes ──────────────────────────────────────────────────────────────

#[test]
fn every_allowlisted_sign_kind_is_accepted() {
    assert_eq!(
        EXTENSION_SIGNABLE_KINDS,
        &[7, 9, 30800, 40003, 45001, 45002, 45003],
        "the v1 signable allowlist is BRIDGE_SPEC.md §4; changing it is a spec change"
    );
    for kind_value in EXTENSION_SIGNABLE_KINDS {
        let scopes =
            format!(r#"{{ "sign": [ {{ "kind": {kind_value}, "channels": ["{CHANNEL_A}"] }} ] }}"#);
        parse_and_validate(&manifest_with_scopes(&scopes))
            .unwrap_or_else(|error| panic!("kind {kind_value} should be signable: {error}"));
    }
}

#[test]
fn rejects_sign_kinds_outside_the_allowlist() {
    // 1 note and 30315 status were dropped from v1; 5 deletion, 9000 add-member,
    // 24134 device-pairing, 30177 managed-agent redefinition and 46020 workflow
    // trigger are never-grantable (decision 003).
    for kind_value in [1u32, 5, 9000, 24134, 30177, 30315, 46020] {
        let scopes =
            format!(r#"{{ "sign": [ {{ "kind": {kind_value}, "channels": ["{CHANNEL_A}"] }} ] }}"#);
        let Err(error) = parse_and_validate(&manifest_with_scopes(&scopes)) else {
            panic!("kind {kind_value} must not be signable");
        };
        assert!(
            error.contains(&format!("scopes.sign requests kind {kind_value}")),
            "got: {error}"
        );
    }
}

#[test]
fn sign_scope_requires_a_non_empty_channel_list() {
    let scopes = r#"{ "sign": [ { "kind": 9, "channels": [] } ] }"#;
    let Err(error) = parse_and_validate(&manifest_with_scopes(scopes)) else {
        panic!("an empty channel list must be rejected");
    };
    assert!(
        error.contains("scopes.sign must list at least one channel")
            && error.contains("all channels"),
        "got: {error}"
    );
}

#[test]
fn sign_scope_rejects_non_uuid_channels() {
    for channel in [
        "all",
        "*",
        "",
        "not-a-uuid",
        "11111111222243338444555555555555",
    ] {
        let scopes = format!(r#"{{ "sign": [ {{ "kind": 9, "channels": ["{channel}"] }} ] }}"#);
        let Err(error) = parse_and_validate(&manifest_with_scopes(&scopes)) else {
            panic!("channel {channel:?} must be rejected");
        };
        assert!(
            error.contains("is not a channel UUID"),
            "got: {error} for {channel:?}"
        );
    }
}

#[test]
fn sign_scope_rejects_non_canonical_uuid_casing() {
    let upper = CHANNEL_A.to_uppercase();
    assert_ne!(
        upper, CHANNEL_A,
        "the fixture UUID must contain hex letters or this test proves nothing"
    );
    let scopes = format!(r#"{{ "sign": [ {{ "kind": 9, "channels": ["{upper}"] }} ] }}"#);
    let Err(error) = parse_and_validate(&manifest_with_scopes(&scopes)) else {
        panic!("an uppercase UUID must be rejected");
    };
    assert!(error.contains("is not a channel UUID"), "got: {error}");
}

// ── Read scopes ──────────────────────────────────────────────────────────────

#[test]
fn read_denylist_floor_covers_the_buzz_core_sets() {
    // Derived from buzz-core's maintained sets rather than copied numbers, so
    // a kind added to either set is covered here the moment it is added.
    let mut denied: Vec<u32> = Vec::new();
    denied.extend_from_slice(kind::AUTHOR_ONLY_KINDS);
    denied.extend_from_slice(kind::P_GATED_KINDS);
    denied.push(kind::KIND_GIFT_WRAP);
    denied.extend([41001, 41010, 41011, 41012]);
    denied.extend([
        kind::KIND_NIP43_MEMBERSHIP_LIST,
        kind::KIND_CHANNEL_SUMMARY,
        kind::KIND_PRESENCE_SNAPSHOT,
    ]);
    assert!(
        denied.len() >= 8,
        "the denylist floor sample looks empty: {denied:?}"
    );

    for kind_value in denied {
        assert!(
            is_read_denied_kind(kind_value),
            "kind {kind_value} must be on the read denylist floor"
        );
        let scopes = format!(
            r#"{{ "read": [ {{ "kinds": [{kind_value}], "channels": ["{CHANNEL_A}"] }} ] }}"#
        );
        let Err(error) = parse_and_validate(&manifest_with_scopes(&scopes)) else {
            panic!("read kind {kind_value} must be rejected");
        };
        assert!(
            error.contains(&format!("scopes.read requests kind {kind_value}")),
            "got: {error}"
        );
    }
}

#[test]
fn read_denylist_rejects_a_denied_kind_mixed_into_an_allowed_list() {
    let scopes = format!(
        r#"{{ "read": [ {{ "kinds": [9, {}], "channels": ["{CHANNEL_A}"] }} ] }}"#,
        kind::KIND_GIFT_WRAP
    );
    let Err(error) = parse_and_validate(&manifest_with_scopes(&scopes)) else {
        panic!("gift wrap mixed into a read scope must be rejected");
    };
    assert!(
        error.contains("scopes.read requests kind 1059"),
        "got: {error}"
    );
}

#[test]
fn read_scope_rejects_extension_data_and_says_where_to_read_it() {
    // BRIDGE_SPEC §5/§7: kind 30800 is never served through
    // `query.events`/`subscribe`; its only read path is `extensionData.get`,
    // gated by the boolean `extensionData` scope. A manifest asking to *read*
    // it is asking for a path that does not exist, so it is rejected at
    // install with a message naming the scope that does work.
    let scopes =
        format!(r#"{{ "read": [ {{ "kinds": [30800], "channels": ["{CHANNEL_A}"] }} ] }}"#);
    let Err(error) = parse_and_validate(&manifest_with_scopes(&scopes)) else {
        panic!("a read grant on kind 30800 must be rejected");
    };
    assert!(
        error.contains("extensionData.get") && error.contains("extensionData"),
        "the error must point at the scope that does work; got: {error}"
    );

    // Mixed into an otherwise-legal list, it must still be caught.
    let mixed =
        format!(r#"{{ "read": [ {{ "kinds": [9, 30800], "channels": ["{CHANNEL_A}"] }} ] }}"#);
    assert!(
        parse_and_validate(&manifest_with_scopes(&mixed)).is_err(),
        "30800 mixed into a read scope must be rejected"
    );

    // 30800 stays *signable* — the write path is `publish.extensionData`.
    assert!(EXTENSION_SIGNABLE_KINDS.contains(&30800));
    assert!(is_read_denied_kind(30800));
}

#[test]
fn read_scope_accepts_ordinary_channel_kinds() {
    let scopes =
        format!(r#"{{ "read": [ {{ "kinds": [9, 45001], "channels": ["{CHANNEL_A}"] }} ] }}"#);
    parse_and_validate(&manifest_with_scopes(&scopes)).expect("kinds 9/45001 should be readable");
}

#[test]
fn read_scope_requires_kinds_and_channels() {
    let scopes = format!(r#"{{ "read": [ {{ "kinds": [], "channels": ["{CHANNEL_A}"] }} ] }}"#);
    let Err(error) = parse_and_validate(&manifest_with_scopes(&scopes)) else {
        panic!("an empty kind list must be rejected");
    };
    assert!(
        error.contains("must list at least one kind"),
        "got: {error}"
    );

    let scopes = r#"{ "read": [ { "kinds": [9], "channels": [] } ] }"#;
    let Err(error) = parse_and_validate(&manifest_with_scopes(scopes)) else {
        panic!("an empty channel list must be rejected");
    };
    assert!(
        error.contains("scopes.read must list at least one channel"),
        "got: {error}"
    );
}

#[test]
fn only_extension_data_is_both_signable_and_read_denied() {
    // A channel-content kind an extension may publish must also be readable
    // back through `query.events`/`subscribe`, or a grant lets it write
    // something it can never see.
    //
    // Kind 30800 is the one deliberate asymmetry (BRIDGE_SPEC §4/§5): it is
    // written with `publish.extensionData` and read with `extensionData.get`,
    // both gated by the `extensionData` scope, and it is never served through
    // the query surface. Naming the exception explicitly keeps this test able
    // to catch an accidental read-denial of any of the other six.
    for kind_value in EXTENSION_SIGNABLE_KINDS {
        if *kind_value == KIND_EXTENSION_DATA {
            assert!(
                is_read_denied_kind(*kind_value),
                "kind 30800 must stay off the query surface"
            );
            continue;
        }
        assert!(
            !is_read_denied_kind(*kind_value),
            "signable kind {kind_value} must not sit on the read denylist floor"
        );
    }
}

// ── Egress ───────────────────────────────────────────────────────────────────

#[test]
fn egress_accepts_bare_origins() {
    for origin in [
        "https://example.com",
        "https://example.com/",
        "http://localhost:5173",
        "wss://relay.example.com:443",
    ] {
        parse_and_validate(&manifest_with_egress(origin))
            .unwrap_or_else(|error| panic!("origin {origin:?} should be accepted: {error}"));
    }
}

#[test]
fn egress_rejects_anything_that_is_not_a_bare_origin() {
    for origin in [
        "https://example.com/steal",
        "https://example.com?q=1",
        "https://example.com#frag",
        "https://user:pw@example.com",
        "example.com",
        "file:///etc/passwd",
        "javascript:alert(1)",
        "https://",
        "",
    ] {
        let Err(error) = parse_and_validate(&manifest_with_egress(origin)) else {
            panic!("origin {origin:?} must be rejected");
        };
        assert!(
            error.contains("egress entry"),
            "got: {error} for {origin:?}"
        );
    }
}

// ── Entry ────────────────────────────────────────────────────────────────────

#[test]
fn rejects_unsafe_entry_paths() {
    for (entry, needle) in [
        ("/etc/passwd", "an absolute path"),
        ("\\\\Windows\\\\evil.html", "an absolute path"),
        ("C:\\\\evil.html", "an absolute path"),
        ("../../evil.html", "path traversal"),
        ("assets\\\\..\\\\..\\\\evil.html", "path traversal"),
        ("", "an empty path"),
    ] {
        let Err(error) = parse_and_validate(&manifest_with("entry", &format!("\"{entry}\"")))
        else {
            panic!("entry {entry:?} must be rejected");
        };
        assert!(
            error.contains("\"entry\"") && error.contains(needle),
            "expected {needle:?} in: {error}"
        );
    }
}

#[test]
fn rejects_entry_naming_a_directory() {
    let Err(error) = parse_and_validate(&manifest_with("entry", "\"assets/\"")) else {
        panic!("a trailing separator must be rejected");
    };
    assert!(error.contains("must name a file"), "got: {error}");
}

#[test]
fn entry_file_must_exist_as_a_regular_file() {
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::write(root.path().join("index.html"), b"<!doctype html>").expect("write entry");
    std::fs::create_dir(root.path().join("assets")).expect("mkdir");

    assert_eq!(validate_entry_file(root.path(), "index.html"), Ok(()));

    let Err(error) = validate_entry_file(root.path(), "missing.html") else {
        panic!("a missing entry file must be rejected");
    };
    assert!(
        error.contains("is missing from the package"),
        "got: {error}"
    );

    let Err(error) = validate_entry_file(root.path(), "assets") else {
        panic!("a directory entry must be rejected");
    };
    assert!(error.contains("not a regular file"), "got: {error}");
}

#[cfg(unix)]
#[test]
fn entry_file_may_not_be_a_symlink() {
    let root = tempfile::tempdir().expect("tempdir");
    std::os::unix::fs::symlink("/etc/passwd", root.path().join("index.html"))
        .expect("create symlink");

    let Err(error) = validate_entry_file(root.path(), "index.html") else {
        panic!("a symlinked entry must be rejected");
    };
    assert!(error.contains("not a regular file"), "got: {error}");
}

#[test]
fn load_and_validate_reads_the_manifest_from_the_package_root() {
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::write(root.path().join(MANIFEST_FILE_NAME), valid_manifest_json())
        .expect("write manifest");
    std::fs::write(root.path().join("index.html"), b"<!doctype html>").expect("write entry");

    let manifest = load_and_validate_manifest(root.path()).expect("manifest should load");
    assert_eq!(manifest.id, "equation-explorer");
}

#[test]
fn load_and_validate_reports_a_missing_manifest() {
    let root = tempfile::tempdir().expect("tempdir");
    let Err(error) = load_and_validate_manifest(root.path()) else {
        panic!("a package with no manifest must be rejected");
    };
    assert!(
        error.contains("has no extension.json at its root"),
        "got: {error}"
    );
}
