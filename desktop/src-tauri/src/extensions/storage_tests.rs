use super::*;

fn authority(identity: &str, digest: &str, generation: u64) -> LeaseAuthority {
    LeaseAuthority {
        extension_id: "equation-explorer".to_string(),
        identity_pubkey: identity.to_string(),
        package_digest: digest.to_string(),
        grant_generation: generation,
    }
}

fn db() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = open_storage_db(&dir.path().join("storage.db")).expect("open");
    (dir, conn)
}

fn revision(reply: &BridgeReply) -> u64 {
    reply
        .result
        .as_ref()
        .and_then(|value| value.get("revision"))
        .and_then(Value::as_u64)
        .expect("revision")
}

#[test]
fn set_get_delete_uses_exact_revision_compare_and_swap() {
    let (_dir, mut conn) = db();
    let owner = authority(&"a".repeat(64), &"b".repeat(64), 1);
    let set_reply = set(
        &mut conn,
        &owner,
        Some(serde_json::json!({"key":"state","value":{"answer":42},"expectedRevision":null})),
    );
    assert!(set_reply.ok);
    assert_eq!(revision(&set_reply), 1);

    let got = get(&conn, &owner, Some(serde_json::json!({"key":"state"})));
    assert_eq!(
        got.result,
        Some(serde_json::json!({"value":{"answer":42},"revision":1}))
    );

    let stale = set(
        &mut conn,
        &owner,
        Some(serde_json::json!({"key":"state","value":{"answer":0},"expectedRevision":null})),
    );
    assert_eq!(stale.error_code(), Some(code::CONFLICT));
    let stale_delete = delete(
        &mut conn,
        &owner,
        Some(serde_json::json!({"key":"state","expectedRevision":2})),
    );
    assert_eq!(stale_delete.error_code(), Some(code::CONFLICT));
    assert!(
        delete(
            &mut conn,
            &owner,
            Some(serde_json::json!({"key":"state","expectedRevision":1})),
        )
        .ok
    );
}

#[test]
fn namespaces_are_isolated_by_identity_digest_and_grant_generation() {
    let (_dir, mut conn) = db();
    let a = authority(&"a".repeat(64), &"d".repeat(64), 1);
    assert!(
        set(
            &mut conn,
            &a,
            Some(serde_json::json!({"key":"state","value":"A","expectedRevision":null})),
        )
        .ok
    );
    for other in [
        authority(&"b".repeat(64), &"d".repeat(64), 1),
        authority(&"a".repeat(64), &"e".repeat(64), 1),
        authority(&"a".repeat(64), &"d".repeat(64), 2),
    ] {
        assert_eq!(
            get(&conn, &other, Some(serde_json::json!({"key":"state"}))).result,
            Some(serde_json::json!({"value":null,"revision":null}))
        );
    }
}

#[test]
fn malformed_keys_unknown_fields_and_oversized_values_fail_closed() {
    let (_dir, mut conn) = db();
    let owner = authority(&"a".repeat(64), &"d".repeat(64), 1);
    for params in [
        serde_json::json!({"key":"../escape","value":1,"expectedRevision":null}),
        serde_json::json!({"key":"state","value":1,"expectedRevision":null,"extensionId":"other"}),
        serde_json::json!({"key":"state","value":1,"expectedRevision":u64::MAX}),
    ] {
        assert_eq!(
            set(&mut conn, &owner, Some(params)).error_code(),
            Some(code::INVALID_PARAMS)
        );
    }
    let oversized = "x".repeat(MAX_VALUE_BYTES + 1);
    assert_eq!(
        set(
            &mut conn,
            &owner,
            Some(serde_json::json!({"key":"state","value":oversized,"expectedRevision":null})),
        )
        .error_code(),
        Some(code::QUOTA_EXCEEDED)
    );
}

#[test]
fn retained_generations_share_a_bounded_disk_quota() {
    let (_dir, mut conn) = db();
    let owner = authority(&"a".repeat(64), &"d".repeat(64), 2);
    conn.execute(
        "INSERT INTO extension_storage
         (identity_pubkey, extension_id, package_digest, grant_generation, storage_key, value_json, revision, updated_at)
         VALUES (?1, ?2, ?3, 1, 'legacy', zeroblob(?4), 1, unixepoch())",
        params![
            owner.identity_pubkey,
            owner.extension_id,
            owner.package_digest,
            MAX_RETAINED_BYTES_PER_EXTENSION
        ],
    )
    .expect("legacy retained bytes");
    assert_eq!(
        set(
            &mut conn,
            &owner,
            Some(serde_json::json!({"key":"state","value":1,"expectedRevision":null})),
        )
        .error_code(),
        Some(code::QUOTA_EXCEEDED)
    );
}

#[test]
fn delete_database_fault_is_internal_and_preserves_the_row() {
    let (_dir, mut conn) = db();
    let owner = authority(&"a".repeat(64), &"d".repeat(64), 2);
    assert!(
        set(
            &mut conn,
            &owner,
            Some(serde_json::json!({"key":"state","value":1,"expectedRevision":null})),
        )
        .ok
    );
    conn.execute_batch(
        "CREATE TRIGGER refuse_storage_delete BEFORE DELETE ON extension_storage
         BEGIN SELECT RAISE(ABORT, 'refuse'); END;",
    )
    .expect("trigger");
    assert_eq!(
        delete(
            &mut conn,
            &owner,
            Some(serde_json::json!({"key":"state","expectedRevision":1})),
        )
        .error_code(),
        Some(code::INTERNAL)
    );
    assert_eq!(
        get(&conn, &owner, Some(serde_json::json!({"key":"state"}))).result,
        Some(serde_json::json!({"value":1,"revision":1}))
    );
}

#[test]
fn storage_dispatch_requires_the_exact_live_scope_and_generation() {
    let app = tauri::test::mock_app();
    let base = super::super::extensions_base_dir(app.handle()).expect("base");
    let extension_id = format!("storage-auth-{}", uuid::Uuid::new_v4().simple());
    let root = base.join(&extension_id);
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(
        root.join("extension.json"),
        serde_json::json!({
            "id": extension_id,
            "name": "Storage authority",
            "version": "1",
            "entry": "index.html",
            "scopes": { "storage": true },
            "egress": []
        })
        .to_string(),
    )
    .expect("manifest");
    std::fs::write(root.join("index.html"), "<!doctype html>").expect("entry");
    let manifest = super::super::manifest::load_and_validate_manifest(&root).expect("manifest");
    let digest = super::super::management::package_digest(&root).expect("digest");
    let identity = "a".repeat(64);
    let db_path = super::super::dispatch::grant_db_path(app.handle()).expect("grant path");
    let mut grants = super::super::grants::open_grant_db(&db_path).expect("grants");
    super::super::grants::delete_all_for_extension(&mut grants, &extension_id).expect("clean");
    super::super::grants::replace_for_identity(
        &mut grants,
        &identity,
        &manifest,
        &digest,
        &Default::default(),
    )
    .expect("ungranted consent");
    let owner = LeaseAuthority {
        extension_id: extension_id.clone(),
        identity_pubkey: identity.clone(),
        package_digest: digest.clone(),
        grant_generation: 1,
    };
    assert_eq!(
        dispatch(
            app.handle(),
            &owner,
            "storage.get",
            Some(serde_json::json!({"key":"state"})),
        )
        .error_code(),
        Some(code::DENIED)
    );

    let selected = super::super::grants::GrantSelection {
        storage: true,
        ..Default::default()
    };
    super::super::grants::replace_for_identity(
        &mut grants,
        &identity,
        &manifest,
        &digest,
        &selected,
    )
    .expect("storage grant");
    let generation =
        super::super::grants::current_generation(&grants, &identity, &extension_id, &digest)
            .expect("generation");
    let current = LeaseAuthority {
        grant_generation: generation,
        ..owner.clone()
    };
    assert!(
        dispatch(
            app.handle(),
            &current,
            "storage.get",
            Some(serde_json::json!({"key":"state"})),
        )
        .ok
    );
    assert_eq!(
        dispatch(
            app.handle(),
            &owner,
            "storage.get",
            Some(serde_json::json!({"key":"state"})),
        )
        .error_code(),
        Some(code::DENIED),
        "a predecessor grant generation must not adopt current storage"
    );

    let write = dispatch(
        app.handle(),
        &current,
        "storage.set",
        Some(serde_json::json!({"key":"state","value":{"private":"old"},"expectedRevision":null})),
    );
    assert!(write.ok);
    super::super::grants::delete_all_for_extension(&mut grants, &extension_id)
        .expect("remove grants but retain generation ledger");
    grants
        .execute(
            "DELETE FROM extension_grant_generations WHERE identity_pubkey = ?1 AND extension_id = ?2",
            rusqlite::params![identity, extension_id],
        )
        .expect("simulate pre-ledger removal");
    let retained_floor = max_retained_generation(app.handle(), &identity, &extension_id)
        .expect("retained storage floor");
    assert_eq!(retained_floor, generation);
    super::super::grants::replace_for_install_with_floor(
        &mut grants,
        &identity,
        &manifest,
        &digest,
        &selected,
        retained_floor,
    )
    .expect("reinstall same bytes");
    let reinstalled_generation =
        super::super::grants::current_generation(&grants, &identity, &extension_id, &digest)
            .expect("reinstalled generation");
    assert!(reinstalled_generation > generation);
    let reinstalled = LeaseAuthority {
        grant_generation: reinstalled_generation,
        ..owner.clone()
    };
    assert_eq!(
        dispatch(
            app.handle(),
            &reinstalled,
            "storage.get",
            Some(serde_json::json!({"key":"state"})),
        )
        .result,
        Some(serde_json::json!({"value":null,"revision":null})),
        "an identical reinstall must not silently adopt the removed installation's state"
    );
    super::super::grants::delete_all_for_extension(&mut grants, &extension_id).expect("cleanup");
    std::fs::remove_dir_all(root).ok();
}
