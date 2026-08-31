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
