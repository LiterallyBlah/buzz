//! What the launch breadcrumb can and cannot say.
//!
//! Both reports of a deaf wake word were the first start after an in-app
//! update, and the updater leaves nothing in the process to detect (see the
//! module docs). The breadcrumb is the fallback, so the one property worth
//! pinning is that it never *claims* an update that did not happen — a false
//! "first launch after an update" would send the next investigation somewhere
//! there is nothing to find.

use super::*;

#[test]
fn a_first_ever_launch_is_not_reported_as_an_update() {
    // Nothing recorded means nothing is known. A fresh install, a cleared
    // application data directory and an unreadable breadcrumb all land here,
    // and none of them is evidence of an update.
    let first = diagnose("0.5.8-unified.11", None, Vec::new());
    assert!(!first.first_launch_after_update);
    assert_eq!(first.previous_version, None);
    assert_eq!(first.version, "0.5.8-unified.11");
}

#[test]
fn an_ordinary_restart_is_not_reported_as_an_update() {
    let restarted = diagnose(
        "0.5.8-unified.11",
        Some("0.5.8-unified.11".to_string()),
        Vec::new(),
    );
    assert!(!restarted.first_launch_after_update);
}

#[test]
fn a_launch_that_follows_another_version_is_the_case_being_watched() {
    let updated = diagnose(
        "0.5.8-unified.11",
        Some("0.5.8-unified.10".to_string()),
        vec!["--flag".to_string()],
    );
    assert!(updated.first_launch_after_update);
    assert_eq!(
        updated.previous_version.as_deref(),
        Some("0.5.8-unified.10")
    );
    assert_eq!(updated.args, vec!["--flag".to_string()]);

    // A downgrade is a version change too, and worth the same flag: what
    // matters to the investigation is that the build on disk changed under a
    // configuration that was already there.
    let downgraded = diagnose(
        "0.5.8-unified.9",
        Some("0.5.8-unified.10".to_string()),
        Vec::new(),
    );
    assert!(downgraded.first_launch_after_update);
}

#[test]
fn the_breadcrumb_survives_one_launch_and_is_replaced_by_the_next() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(LAUNCH_FILE);

    // Nothing to compare against on the first launch, and the file appears.
    assert_eq!(exchange_recorded_version(&path, "0.5.8-unified.10"), None);
    assert!(path.is_file());

    // The next launch of the same build reads itself back — no update.
    assert_eq!(
        exchange_recorded_version(&path, "0.5.8-unified.10").as_deref(),
        Some("0.5.8-unified.10")
    );

    // And a launch of a different build sees the one it replaced, which is the
    // whole signal.
    assert_eq!(
        exchange_recorded_version(&path, "0.5.8-unified.11").as_deref(),
        Some("0.5.8-unified.10")
    );
    assert_eq!(
        exchange_recorded_version(&path, "0.5.8-unified.11").as_deref(),
        Some("0.5.8-unified.11")
    );
}

#[test]
fn an_unreadable_breadcrumb_is_not_an_update_and_not_an_error() {
    // A truncated or hand-edited file must degrade to "nothing is known", never
    // to a launch that refuses to start or a diagnostic that invents a version.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(LAUNCH_FILE);
    std::fs::write(&path, b"{ not json").expect("write");

    assert_eq!(exchange_recorded_version(&path, "0.5.8-unified.11"), None);
    // …and it was repaired in passing, so the launch after this one works.
    assert_eq!(
        exchange_recorded_version(&path, "0.5.8-unified.11").as_deref(),
        Some("0.5.8-unified.11")
    );
}
