use std::sync::Arc;

use super::frame_host_test_support::{installed, set_acquire_install_hook, AcquireInstallHook};
use super::*;

#[tokio::test]
async fn shutdown_wins_deterministically_over_a_late_open_install() {
    let _guard = lifecycle_guard().await;
    let base = installed(&[
        (
            "extension.json",
            br#"{"id":"demo","name":"Demo","version":"1","entry":"index.html"}"#,
        ),
        ("index.html", b"<!doctype html>"),
    ]);
    let hook = Arc::new(AcquireInstallHook {
        reached: tokio::sync::Notify::new(),
        proceed: tokio::sync::Notify::new(),
    });
    set_acquire_install_hook(Some(Arc::clone(&hook)));

    let base_dir = base.path().to_path_buf();
    let opening = tokio::spawn(async move { acquire(base_dir, "demo").await });
    hook.reached.notified().await;
    shutdown_now();
    hook.proceed.notify_one();
    let result = opening.await.expect("join");
    set_acquire_install_hook(None);

    assert!(result.is_err(), "a stale opening epoch cannot mint a lease");
    assert!(
        running_port().is_none(),
        "no late listener survives shutdown"
    );
}

#[tokio::test]
async fn disable_targets_only_the_matching_identity_and_extension() {
    let _guard = lifecycle_guard().await;
    insert_authorized_lease_for_test("lease-a", "demo", "identity-a", "digest");
    insert_authorized_lease_for_test("lease-b", "demo", "identity-b", "digest");
    insert_authorized_lease_for_test("lease-c", "other", "identity-a", "digest");

    assert_eq!(release_for_identity_extension("identity-a", "demo"), 1);
    assert!(extension_for_lease("lease-a").is_none());
    assert_eq!(extension_for_lease("lease-b").as_deref(), Some("demo"));
    assert_eq!(extension_for_lease("lease-c").as_deref(), Some("other"));
    assert_eq!(release_for_extension_id("demo"), 1);
    assert!(extension_for_lease("lease-b").is_none());
    assert_eq!(extension_for_lease("lease-c").as_deref(), Some("other"));
}

#[tokio::test]
async fn opaque_static_contexts_isolate_same_extension_owners() {
    let _guard = lifecycle_guard().await;
    let base = installed(&[("index.html", b"<!doctype html>")]);
    let digest =
        super::super::management::package_digest(&base.path().join("demo")).expect("digest");
    let a = acquire_authorized(
        base.path().to_path_buf(),
        "demo",
        "identity-a",
        &digest,
        "index.html",
        Vec::new(),
    )
    .await
    .expect("A");
    let b = acquire_authorized(
        base.path().to_path_buf(),
        "demo",
        "identity-b",
        &digest,
        "index.html",
        vec!["https://b.example".into()],
    )
    .await
    .expect("B");
    assert_ne!(a.static_context, b.static_context);
    let owner_a = static_owner(&a.static_context, &digest, "demo").expect("A owner");
    let owner_b = static_owner(&b.static_context, &digest, "demo").expect("B owner");
    assert!(owner_a.egress.is_empty());
    assert_eq!(owner_b.egress, vec!["https://b.example"]);
    assert!(static_owner(&a.static_context, &digest, "other").is_none());
    assert!(static_owner(&a.static_context, "wrong-digest", "demo").is_none());
    assert!(static_owner("unknown", &digest, "demo").is_none());
    release(&a.lease);
    assert!(static_owner(&a.static_context, &digest, "demo").is_none());
    assert!(static_owner(&b.static_context, &digest, "demo").is_some());
}

#[tokio::test]
async fn final_reinstall_sweep_invalidates_an_inflight_old_digest_open() {
    let _guard = lifecycle_guard().await;
    let base = installed(&[("index.html", b"old")]);
    let old_digest =
        super::super::management::package_digest(&base.path().join("demo")).expect("digest");
    let hook = Arc::new(AcquireInstallHook {
        reached: tokio::sync::Notify::new(),
        proceed: tokio::sync::Notify::new(),
    });
    set_acquire_install_hook(Some(Arc::clone(&hook)));
    let base_dir = base.path().to_path_buf();
    let opening = tokio::spawn(async move {
        acquire_authorized(
            base_dir,
            "demo",
            "identity-a",
            &old_digest,
            "index.html",
            Vec::new(),
        )
        .await
    });
    hook.reached.notified().await;
    assert_eq!(release_for_extension_id("demo"), 0, "final exact sweep");
    std::fs::write(base.path().join("demo/index.html"), "new").expect("swap witness");
    hook.proceed.notify_one();
    let result = opening.await.expect("join");
    set_acquire_install_hook(None);
    assert!(
        result.is_err(),
        "the old-digest open must lose the reinstall fence"
    );
    assert!(host_state().leases.is_empty(), "no stale owner survives");
}

#[tokio::test]
async fn repeated_static_admission_performs_zero_package_tree_walks() {
    let _guard = lifecycle_guard().await;
    let base = installed(&[("index.html", b"old"), ("asset.js", b"one")]);
    let digest =
        super::super::management::package_digest(&base.path().join("demo")).expect("digest");
    let claim = acquire_authorized(
        base.path().to_path_buf(),
        "demo",
        "identity-a",
        &digest,
        "index.html",
        Vec::new(),
    )
    .await
    .expect("frame");
    super::super::management::reset_package_tree_walks(&base.path().join("demo"));
    for _ in 0..50 {
        assert!(static_owner(&claim.static_context, &digest, "demo").is_some());
    }
    assert_eq!(super::super::management::package_tree_walks(), 0);
    std::fs::write(base.path().join("demo/asset.js"), "two").expect("witness");
    assert!(static_owner(&claim.static_context, &digest, "demo").is_some());
    assert_eq!(super::super::management::package_tree_walks(), 0);
}
