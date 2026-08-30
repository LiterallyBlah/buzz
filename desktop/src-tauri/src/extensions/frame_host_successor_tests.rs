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
