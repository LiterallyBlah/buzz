//! Tests for the extension grant store.

use super::*;

fn temp_db() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("grants").join("extension-grants.db");
    (dir, path)
}

#[test]
fn an_ungranted_scope_is_denied() {
    let (_dir, path) = temp_db();
    let conn = open_grant_db(&path).expect("open");
    // Nothing has been granted. The store must not invent permission.
    assert!(!has_scope(&conn, &"a".repeat(64), "demo", SCOPE_IDENTITY));
}

#[test]
fn a_granted_scope_is_allowed() {
    let (_dir, path) = temp_db();
    let conn = open_grant_db(&path).expect("open");
    let identity = "a".repeat(64);
    grant_boolean_scope(&conn, &identity, "demo", SCOPE_IDENTITY).expect("grant");
    assert!(has_scope(&conn, &identity, "demo", SCOPE_IDENTITY));
}

#[test]
fn a_grant_survives_reopening_the_database() {
    // The durable readback: an in-memory call proves nothing about a store
    // whose whole job is to persist a user's decision across sessions.
    let (_dir, path) = temp_db();
    let identity = "a".repeat(64);
    {
        let conn = open_grant_db(&path).expect("open");
        grant_boolean_scope(&conn, &identity, "demo", SCOPE_IDENTITY).expect("grant");
    } // connection dropped — the file is the only thing carrying the grant

    let reopened = open_grant_db(&path).expect("reopen");
    assert!(
        has_scope(&reopened, &identity, "demo", SCOPE_IDENTITY),
        "a grant must outlive the connection that recorded it"
    );
}

#[test]
fn a_grant_is_scoped_to_one_extension() {
    let (_dir, path) = temp_db();
    let conn = open_grant_db(&path).expect("open");
    let identity = "a".repeat(64);
    grant_boolean_scope(&conn, &identity, "demo", SCOPE_IDENTITY).expect("grant");
    assert!(
        !has_scope(&conn, &identity, "other-extension", SCOPE_IDENTITY),
        "one extension's grant must not answer for another's"
    );
}

#[test]
fn a_grant_is_scoped_to_one_identity() {
    // The same installed package under a different Buzz identity has been
    // granted nothing by *that* user.
    let (_dir, path) = temp_db();
    let conn = open_grant_db(&path).expect("open");
    grant_boolean_scope(&conn, &"a".repeat(64), "demo", SCOPE_IDENTITY).expect("grant");
    assert!(
        !has_scope(&conn, &"b".repeat(64), "demo", SCOPE_IDENTITY),
        "a grant must not carry across identities"
    );
}

#[test]
fn one_scope_does_not_imply_another() {
    let (_dir, path) = temp_db();
    let conn = open_grant_db(&path).expect("open");
    let identity = "a".repeat(64);
    grant_boolean_scope(&conn, &identity, "demo", "storage").expect("grant");
    assert!(
        !has_scope(&conn, &identity, "demo", SCOPE_IDENTITY),
        "granting storage must not grant identity"
    );
}

#[test]
fn revocation_takes_effect_immediately_and_durably() {
    let (_dir, path) = temp_db();
    let identity = "a".repeat(64);
    let conn = open_grant_db(&path).expect("open");
    grant_boolean_scope(&conn, &identity, "demo", SCOPE_IDENTITY).expect("grant");
    assert!(has_scope(&conn, &identity, "demo", SCOPE_IDENTITY));

    let removed = revoke_all(&conn, &identity, "demo").expect("revoke");
    assert_eq!(removed, 1);
    assert!(!has_scope(&conn, &identity, "demo", SCOPE_IDENTITY));

    drop(conn);
    let reopened = open_grant_db(&path).expect("reopen");
    assert!(
        !has_scope(&reopened, &identity, "demo", SCOPE_IDENTITY),
        "a revoked grant must not come back when the store is reopened"
    );
}

// ── sign scopes: (kind, channel) qualified ───────────────────────────────────

#[test]
fn a_sign_grant_is_scoped_to_its_kind_and_channel() {
    let (_dir, path) = temp_db();
    let conn = open_grant_db(&path).expect("open");
    let identity = "a".repeat(64);
    grant_sign_scope(&conn, &identity, "demo", 9, "channel-a").expect("grant");

    assert!(has_sign_scope(&conn, &identity, "demo", 9, "channel-a"));
    assert!(
        !has_sign_scope(&conn, &identity, "demo", 9, "channel-b"),
        "a grant in one channel must not authorise another"
    );
    assert!(
        !has_sign_scope(&conn, &identity, "demo", 7, "channel-a"),
        "a grant for one kind must not authorise another"
    );
    assert!(
        !has_sign_scope(&conn, &identity, "other", 9, "channel-a"),
        "one extension's sign grant must not answer for another's"
    );
    assert!(
        !has_sign_scope(&conn, &"b".repeat(64), "demo", 9, "channel-a"),
        "a sign grant must not carry across identities"
    );
}

#[test]
fn two_channels_means_two_rows_with_no_wildcard() {
    // §7 forbids an "all channels" sentinel. Granting two channels is two
    // rows, and there is no value of `channel` that means "any".
    let (_dir, path) = temp_db();
    let conn = open_grant_db(&path).expect("open");
    let identity = "a".repeat(64);
    grant_sign_scope(&conn, &identity, "demo", 9, "channel-a").expect("grant");
    grant_sign_scope(&conn, &identity, "demo", 9, "channel-b").expect("grant");

    assert!(has_sign_scope(&conn, &identity, "demo", 9, "channel-a"));
    assert!(has_sign_scope(&conn, &identity, "demo", 9, "channel-b"));
    for pretender in ["", "*", "all", "%"] {
        assert!(
            !has_sign_scope(&conn, &identity, "demo", 9, pretender),
            "{pretender:?} must not read as a wildcard"
        );
    }
}

#[test]
fn a_boolean_grant_is_not_readable_as_a_sign_grant() {
    // The two row shapes share a table. A boolean row stores kind -1 and
    // channel '', and must never satisfy a sign lookup — nor the reverse.
    let (_dir, path) = temp_db();
    let conn = open_grant_db(&path).expect("open");
    let identity = "a".repeat(64);
    grant_boolean_scope(&conn, &identity, "demo", SCOPE_SIGN).expect("grant");

    assert!(
        !has_sign_scope(&conn, &identity, "demo", 9, "channel-a"),
        "a boolean row under the same scope name must not authorise signing"
    );

    grant_sign_scope(&conn, &identity, "demo", 9, "channel-a").expect("grant");
    assert!(
        !has_scope(&conn, &identity, "demo", SCOPE_IDENTITY),
        "a sign grant must not satisfy a boolean scope lookup"
    );
}

#[test]
fn revoking_removes_sign_grants_too() {
    // Revocation is per extension, not per scope shape — a half-revoked
    // extension that could still sign would be worse than no revocation.
    let (_dir, path) = temp_db();
    let conn = open_grant_db(&path).expect("open");
    let identity = "a".repeat(64);
    grant_boolean_scope(&conn, &identity, "demo", SCOPE_IDENTITY).expect("grant");
    grant_sign_scope(&conn, &identity, "demo", 9, "channel-a").expect("grant");
    grant_sign_scope(&conn, &identity, "demo", 7, "channel-a").expect("grant");

    let removed = revoke_all(&conn, &identity, "demo").expect("revoke");
    assert_eq!(removed, 3);

    drop(conn);
    let reopened = open_grant_db(&path).expect("reopen");
    assert!(!has_sign_scope(
        &reopened,
        &identity,
        "demo",
        9,
        "channel-a"
    ));
    assert!(!has_sign_scope(
        &reopened,
        &identity,
        "demo",
        7,
        "channel-a"
    ));
    assert!(!has_scope(&reopened, &identity, "demo", SCOPE_IDENTITY));
}

#[test]
fn a_kind_qualified_lookup_does_not_match_a_boolean_grant() {
    // The stored `-1`/`''` for a boolean scope must read as *not qualified*,
    // never as *any kind, any channel*. §7 forbids "all channels" sentinels,
    // so a future scoped lookup must not be satisfied by a boolean row.
    let (_dir, path) = temp_db();
    let conn = open_grant_db(&path).expect("open");
    let identity = "a".repeat(64);
    grant_boolean_scope(&conn, &identity, "demo", "read").expect("grant");

    let qualified: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM extension_grants
              WHERE identity_pubkey = ?1 AND extension_id = ?2 AND scope = 'read'
                AND kind = 9 AND channel = 'some-channel-uuid'",
            rusqlite::params![identity, "demo"],
            |row| row.get(0),
        )
        .expect("query");
    assert_eq!(
        qualified, 0,
        "a boolean row must not satisfy a (kind, channel) lookup"
    );
}
