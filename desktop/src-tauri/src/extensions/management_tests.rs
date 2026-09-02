use super::*;

fn package(root: &Path, id: &str, body: &str) {
    fs::create_dir_all(root).expect("root");
    fs::write(
        root.join("extension.json"),
        format!(
            r#"{{"id":"{id}","name":"Demo","version":"1","entry":"index.html","scopes":{{"identity":true}},"egress":[]}}"#
        ),
    )
    .expect("manifest");
    fs::write(root.join("index.html"), body).expect("entry");
}

#[test]
fn package_digest_binds_paths_and_exact_bytes() {
    let root = tempfile::tempdir().expect("root");
    package(root.path(), "demo", "first");
    let first = package_digest(root.path()).expect("digest");
    fs::write(root.path().join("index.html"), "other").expect("same-length mutate");
    let second = package_digest(root.path()).expect("digest");
    assert_ne!(first, second);
    fs::write(root.path().join("extra.js"), "second").expect("extra");
    assert_ne!(second, package_digest(root.path()).expect("digest"));
}

#[tokio::test]
async fn repeated_delivery_revalidation_performs_zero_package_tree_walks() {
    let _guard = super::super::frame_host::lifecycle_guard().await;
    let db_root = tempfile::tempdir().expect("db root");
    let package_root = db_root.path().join("delivery-frozen");
    package(&package_root, "delivery-frozen", "frozen");
    let db_path = db_root.path().join(".grants/extension-grants.db");
    let mut conn = super::super::grants::open_grant_db(&db_path).expect("db");
    let keys = nostr::Keys::generate();
    let identity = keys.public_key().to_hex();
    let manifest = ExtensionManifest {
        id: "delivery-frozen".into(),
        name: "Delivery frozen".into(),
        version: "1".into(),
        entry: "index.html".into(),
        scopes: Default::default(),
        egress: Vec::new(),
    };
    let digest = "ab".repeat(32);
    super::super::grants::replace_for_identity(
        &mut conn,
        &identity,
        &manifest,
        &digest,
        &GrantSelection::default(),
    )
    .expect("consent");
    super::super::grants::set_enabled(&conn, &identity, &manifest.id, &digest, true)
        .expect("enable");
    let generation =
        super::super::grants::current_generation(&conn, &identity, &manifest.id, &digest)
            .expect("generation");
    super::super::frame_host::insert_authorized_lease_with_generation_for_test(
        "delivery-lease",
        &manifest.id,
        &identity,
        &digest,
        generation,
    );
    let state = crate::app_state::build_app_state();
    *state.keys.lock().expect("keys") = keys;
    reset_package_tree_walks(&package_root);
    for _ in 0..50 {
        assert!(revalidation_current(
            &state,
            Some(&db_path),
            "delivery-lease",
            &manifest.id,
            &identity,
        ));
    }
    assert_eq!(package_tree_walks(), 0);

    // A grant replacement preserves enabled=true but advances the durable
    // generation. The old lease must fail centrally before any method-specific
    // grant path can observe or return data.
    super::super::grants::replace_for_identity(
        &mut conn,
        &identity,
        &manifest,
        &digest,
        &GrantSelection::default(),
    )
    .expect("replace grants");
    assert!(!revalidation_current(
        &state,
        Some(&db_path),
        "delivery-lease",
        &manifest.id,
        &identity,
    ));
    super::super::frame_host::release("delivery-lease");
}

#[test]
fn prepared_bytes_are_detached_from_source_and_token_is_one_use() {
    clear_prepared();
    let base = tempfile::tempdir().expect("base");
    let source = tempfile::tempdir().expect("source");
    package(source.path(), "demo", "reviewed");
    let prepared =
        prepare_in(base.path(), source.path(), "directory", "identity-a".into()).expect("prepare");

    fs::write(source.path().join("index.html"), "mutated later").expect("mutate source");
    assert!(take_prepared(&prepared.token, "identity-b").is_err());
    let package = take_prepared(&prepared.token, "identity-a").expect("owner consumes");
    assert_eq!(
        fs::read_to_string(package.staged_path.join("index.html")).expect("staged"),
        "reviewed"
    );
    assert_eq!(package.digest, prepared.digest);
    assert!(take_prepared(&prepared.token, "identity-a").is_err());
    remove_staged(&package.staged_path);
    clear_prepared();
}

#[test]
fn explicit_selection_defaults_to_none_and_rejects_invented_authority() {
    let manifest = ExtensionManifest {
        id: "demo".into(),
        name: "Demo".into(),
        version: "1".into(),
        entry: "index.html".into(),
        scopes: super::super::manifest::ExtensionScopes {
            identity: true,
            ..Default::default()
        },
        egress: vec!["https://example.com".into()],
    };
    assert!(
        super::super::grants::validate_selection(&manifest, &GrantSelection::default()).is_ok()
    );
    let invented = GrantSelection {
        sign: vec![super::super::grants::GrantPair {
            kind: 9,
            channel: "c8fb8f44-993d-4166-810e-ebdad7b8b944".into(),
        }],
        ..Default::default()
    };
    assert!(super::super::grants::validate_selection(&manifest, &invented).is_err());
}

#[test]
fn expired_preparation_is_removed_and_cannot_be_replayed() {
    clear_prepared();
    let base = tempfile::tempdir().expect("base");
    let source = tempfile::tempdir().expect("source");
    package(source.path(), "expired", "reviewed");
    let prepared =
        prepare_in(base.path(), source.path(), "directory", "identity-a".into()).expect("prepare");
    let staged = prepared_registry()
        .get(&prepared.token)
        .map(|package| package.staged_path.clone())
        .expect("registered");
    if let Some(package) = prepared_registry().get_mut(&prepared.token) {
        package.expires_at = 0;
    }
    expire_prepared();
    assert!(!staged.exists());
    assert!(take_prepared(&prepared.token, "identity-a").is_err());
}

#[test]
fn removal_parks_only_a_valid_owned_directory() {
    let base = tempfile::tempdir().expect("base");
    let installed = base.path().join("demo");
    package(&installed, "demo", "body");
    let parked = park_extension_for_removal(base.path(), "demo").expect("park");
    assert!(!installed.exists());
    assert!(parked.starts_with(base.path()));
    assert!(parked
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(".removed-")));
    assert!(park_extension_for_removal(base.path(), "../escape").is_err());
    remove_staged(&parked);
}

#[test]
fn committed_hello_world_zip_prepares_into_host_owned_bytes() {
    clear_prepared();
    let base = tempfile::tempdir().expect("base");
    let archive = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/e2e/fixtures/hello-world-extension.zip");
    let prepared = prepare_in(base.path(), &archive, "zip", "identity-a".into())
        .expect("prepare committed zip");
    assert_eq!(prepared.manifest.id, "hello-world");
    assert_eq!(prepared.digest.len(), 64);
    let package = take_prepared(&prepared.token, "identity-a").expect("consume");
    assert_eq!(
        package_digest(&package.staged_path).expect("digest"),
        prepared.digest
    );
    remove_staged(&package.staged_path);
}
