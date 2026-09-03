use super::*;

fn authority(identity: &str, digest: &str, generation: u64) -> LeaseAuthority {
    LeaseAuthority {
        extension_id: "equation-explorer".to_string(),
        identity_pubkey: identity.to_string(),
        package_digest: digest.to_string(),
        grant_generation: generation,
    }
}

fn insert_record(
    authority: &LeaseAuthority,
    label: &str,
    lease: &str,
    wrapper_url: &str,
    data_directory: PathBuf,
    state_value: NativeWindowState,
) {
    let key = SurfaceKey::from(authority);
    let mut state = registry();
    state.by_surface.insert(key.clone(), label.to_string());
    state.by_label.insert(
        label.to_string(),
        NativeRecord {
            key,
            label: label.to_string(),
            lease: lease.to_string(),
            wrapper_url: wrapper_url.to_string(),
            data_directory,
            state: state_value,
            error: None,
            stream_sink: None,
        },
    );
}

fn batch(lease: &str, sub: &str) -> super::super::query::StreamBatch {
    super::super::query::StreamBatch {
        generation: lease.to_string(),
        sub: sub.to_string(),
        seq: 1,
        token: "99999999-9999-4999-8999-999999999999".to_string(),
        frames: vec![serde_json::json!({ "sub": sub, "kind": "eose" })],
        frame_count: 1,
        encoded_bytes: 28,
        terminal: false,
    }
}

#[test]
fn accepted_all_frame_script_bytes_and_constructor_list_are_exact() {
    assert_eq!(
        hex::encode(Sha256::digest(WEBRTC_DISABLE_SCRIPT.as_bytes())),
        ACCEPTED_SCRIPT_SHA256
    );
    for name in [
        "RTCPeerConnection",
        "webkitRTCPeerConnection",
        "mozRTCPeerConnection",
        "RTCDataChannel",
        "webkitRTCDataChannel",
        "mozRTCDataChannel",
    ] {
        assert_eq!(
            WEBRTC_DISABLE_SCRIPT
                .matches(&format!("\"{name}\""))
                .count(),
            1,
            "{name}"
        );
    }
    assert!(WEBRTC_DISABLE_SCRIPT.contains("writable: false"));
    assert!(WEBRTC_DISABLE_SCRIPT.contains("configurable: false"));
}

#[test]
fn observed_windows_udf_hierarchy_is_shortened_below_the_product_budget() {
    let root = Path::new(r"C:\Users\micha\AppData\Roaming\xyz.block.buzz.app\extension-webview2");
    let identity = "00".repeat(32);
    let digest = "9009ff7bc3aa039b3c85c5e3b74333acf493260f181a26fd69975dd1c37e74d4";
    let label = "extension-secure-00000000-0000-4000-8000-000000000000";
    let old_data_directory = root
        .join(hex::encode(Sha256::digest(identity.as_bytes())))
        .join("equation-explorer")
        .join(digest)
        .join("2")
        .join(label);
    assert_eq!(path_utf16_units(&old_data_directory), 272);
    assert!(path_utf16_units(&old_data_directory) > 260);

    let plan = plan_native_window(
        root,
        authority(&identity, digest, 2),
        label.to_string(),
        "http://127.0.0.1:3001/frame/context/digest/equation-explorer".to_string(),
    )
    .unwrap();
    assert_eq!(path_utf16_units(&plan.data_directory), 133);
    assert!(path_utf16_units(&plan.data_directory) <= NATIVE_UDF_MAX_UTF16_UNITS);
}

#[test]
fn udf_is_one_opaque_direct_child_without_raw_authority_segments() {
    let root = Path::new("C:/Buzz/extension-webview2");
    let identity = "11".repeat(32);
    let digest = "ab".repeat(32);
    let label = "extension-secure-first";
    let plan = plan_native_window(
        root,
        authority(&identity, &digest, 7),
        label.to_string(),
        "http://127.0.0.1:3001/frame/context/digest/equation-explorer".to_string(),
    )
    .unwrap();
    assert_eq!(plan.data_directory.parent(), Some(root));
    let leaf = plan.data_directory.file_name().unwrap().to_string_lossy();
    assert_eq!(leaf.len(), 64);
    assert!(leaf.bytes().all(|byte| byte.is_ascii_hexdigit()));
    for raw_segment in [
        identity.as_str(),
        "equation-explorer",
        digest.as_str(),
        "7",
        label,
    ] {
        assert_ne!(leaf, raw_segment);
    }
}

#[test]
fn udf_is_deterministic_and_changes_when_each_bound_field_changes() {
    let root = Path::new("C:/Buzz/extension-webview2");
    let digest = "ab".repeat(32);
    let label = "extension-secure-first";
    let base_authority = authority("identity-a", &digest, 7);
    let derive = |authority: LeaseAuthority, label: &str| {
        plan_native_window(
            root,
            authority,
            label.to_string(),
            "http://127.0.0.1:3001/frame/context/digest/equation-explorer".to_string(),
        )
        .unwrap()
        .data_directory
    };
    let base = derive(base_authority.clone(), label);
    assert_eq!(base, derive(base_authority.clone(), label));

    let mut changed_extension = base_authority.clone();
    changed_extension.extension_id = "equation-viewer".to_string();
    let mut changed_digest = base_authority.clone();
    changed_digest.package_digest = "cd".repeat(32);
    for changed in [
        derive(authority("identity-b", &digest, 7), label),
        derive(changed_extension, label),
        derive(changed_digest, label),
        derive(authority("identity-a", &digest, 8), label),
        derive(base_authority.clone(), "extension-secure-second"),
    ] {
        assert_ne!(base, changed);
    }

    let mut reassigned_boundary_a = authority("alpha", &digest, 7);
    reassigned_boundary_a.extension_id = "bc".to_string();
    let mut reassigned_boundary_b = authority("alphab", &digest, 7);
    reassigned_boundary_b.extension_id = "c".to_string();
    assert_eq!(
        format!(
            "{}{}",
            reassigned_boundary_a.identity_pubkey, reassigned_boundary_a.extension_id
        ),
        format!(
            "{}{}",
            reassigned_boundary_b.identity_pubkey, reassigned_boundary_b.extension_id
        )
    );
    assert_ne!(
        derive(reassigned_boundary_a, label),
        derive(reassigned_boundary_b, label)
    );
}

#[test]
fn unusually_long_udf_root_fails_before_window_creation() {
    let root = PathBuf::from(format!("C:/{}", "r".repeat(100)));
    let error = plan_native_window(
        &root,
        authority("identity-a", &"ab".repeat(32), 7),
        "extension-secure-first".to_string(),
        "http://127.0.0.1:3001/frame/context/digest/equation-explorer".to_string(),
    )
    .unwrap_err();
    assert_eq!(
        error,
        "secure extension data path exceeds Buzz's supported Windows path budget of 160 UTF-16 code units"
    );
}

#[test]
fn production_plan_contains_no_measurement_browser_arguments() {
    let source = include_str!("native_window.rs");
    let production = source.split("#[cfg(test)]").next().unwrap();
    let ignored_cert = ["ignore", "certificate", "errors"].join("-");
    let proxy_flag = ["disable", "non", "proxied", "udp"].join("_");
    let browser_args = ["additional", "browser", "args"].join("_");
    assert!(!production.contains(&ignored_cert));
    assert!(!production.contains(&proxy_flag));
    assert!(!production.contains(&browser_args));
    assert_eq!(
        production
            .matches("initialization_script_for_all_frames")
            .count(),
        1
    );
    assert_eq!(
        production
            .matches("data_directory(plan.data_directory.clone())")
            .count(),
        1
    );
    assert!(!include_str!("../huddle/window.rs").contains("initialization_script_for_all_frames"));
}

#[tokio::test]
async fn registry_and_cleanup_retain_the_exact_planned_udf() {
    let _guard = super::super::frame_host::lifecycle_guard().await;
    let app = tauri::test::mock_app();
    close_all(app.handle());
    let root = tempfile::tempdir().unwrap();
    let identity = "22".repeat(32);
    let digest = "bc".repeat(32);
    let authority = authority(&identity, &digest, 9);
    let label = "extension-secure-planned-cleanup";
    let lease = "77777777-7777-4777-8777-777777777777";
    let wrapper_url = "http://127.0.0.1:41001/frame/context/digest/equation-explorer";
    let plan = plan_native_window(
        root.path(),
        authority.clone(),
        label.to_string(),
        wrapper_url.to_string(),
    )
    .unwrap();
    std::fs::create_dir_all(&plan.data_directory).unwrap();
    std::fs::write(plan.data_directory.join("owned.txt"), "owned").unwrap();
    insert_record(
        &authority,
        label,
        lease,
        wrapper_url,
        plan.data_directory.clone(),
        NativeWindowState::Opening,
    );
    assert_eq!(
        registry().by_label.get(label).unwrap().data_directory,
        plan.data_directory
    );

    cleanup_record(app.handle(), label, false, NativeWindowState::Closed, None);
    for _ in 0..50 {
        if !plan.data_directory.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(!plan.data_directory.exists());
    assert!(root.path().exists());
    assert!(registry().by_label.is_empty());
}

#[test]
fn linux_surface_mode_retains_the_iframe_path() {
    #[cfg(not(target_os = "windows"))]
    assert_eq!(
        ExtensionSurfaceMode::current(),
        ExtensionSurfaceMode::LinuxIframe
    );
}

#[test]
fn native_wrapper_originates_one_channel_and_rejects_frame_ports() {
    let source = include_str!("native_wrapper.js");
    assert_eq!(source.matches("new MessageChannel()").count(), 1);
    assert!(!source.contains("event.ports"));
    assert!(source.contains("event.source !== frame.contentWindow"));
    assert!(source.contains("plugin:extension-bridge|invoke"));
    assert!(source.contains("plugin:extension-bridge|stream_control"));
    assert!(source.contains("plugin:extension-bridge|native_ready"));
}

#[test]
fn wrapper_policy_is_platform_specific_and_host_derived() {
    let origin = "http://127.0.0.1:43123";
    let linux = super::super::frame_host::wrapper_content_security_policy_for_mode(
        origin,
        super::super::frame_authority::WrapperMode::LinuxIframe,
    );
    let windows = super::super::frame_host::wrapper_content_security_policy_for_mode(
        origin,
        super::super::frame_authority::WrapperMode::WindowsTopLevel,
    );
    assert!(!linux.contains("frame-ancestors"));
    assert!(windows.contains("frame-ancestors 'none'"));
    assert!(linux.contains(&format!("frame-src {origin}")));
    assert!(windows.contains(&format!("frame-src {origin}")));
}

#[test]
fn terminal_pass_fixture_pins_initial_srcdoc_controls_and_server_snapshot() {
    let result_bytes =
        include_bytes!("../../tests/fixtures/extensions/webview2-realm-disable-results.json");
    assert_eq!(
        hex::encode(Sha256::digest(result_bytes)),
        "41238014f32883efcae15b33b8c886d262f4cd560705da0648b5094b5f6d96f3"
    );
    let result: serde_json::Value = serde_json::from_slice(result_bytes).unwrap();
    assert_eq!(result["overall"], "PASS");
    assert_eq!(result["injected_script_sha256"], ACCEPTED_SCRIPT_SHA256);
    assert_eq!(result["rows"].as_array().unwrap().len(), 1);
    let evidence = &result["rows"][0]["evidence"];
    for field in [
        "matrix_complete",
        "snapshots_valid",
        "candidate_off_reports_live",
        "huddle_report_live",
        "loopback_controls_live",
        "offhost_controls_live",
        "protected_reports_blocked",
        "protected_sinks_zero",
    ] {
        assert_eq!(evidence[field], true, "{field}");
    }
    for lane in ["protected-initial", "protected-srcdoc"] {
        let report = &evidence["reports"][lane];
        for constructor in [
            "RTCPeerConnection",
            "webkitRTCPeerConnection",
            "mozRTCPeerConnection",
            "RTCDataChannel",
            "webkitRTCDataChannel",
            "mozRTCDataChannel",
        ] {
            assert_eq!(report["constructorTypes"][constructor], "undefined");
        }
    }
    let snapshot_bytes =
        include_bytes!("../../tests/fixtures/extensions/offhost-snapshot-at-intake.json");
    assert_eq!(
        hex::encode(Sha256::digest(snapshot_bytes)),
        "ae0e926765e9e34ce778269dc36581ab55bf44a6c978a4d40c7ae91562f8ff44"
    );
    let snapshot: serde_json::Value = serde_json::from_slice(snapshot_bytes).unwrap();
    assert_eq!(evidence["offhost_snapshot"], snapshot);
}

#[tokio::test]
async fn lifecycle_close_is_exact_idempotent_and_reopen_uses_fresh_authority() {
    let _guard = super::super::frame_host::lifecycle_guard().await;
    let app = tauri::test::mock_app();
    let identity = "11".repeat(32);
    let digest = "ab".repeat(32);
    let extension_id = "equation-explorer";
    let first_label = "extension-secure-lifecycle-first".to_string();
    let first_lease = "44444444-4444-4444-8444-444444444444";
    super::super::frame_host::insert_authorized_lease_with_generation_for_test(
        first_lease,
        extension_id,
        &identity,
        &digest,
        9,
    );
    let first_key = SurfaceKey {
        identity_pubkey: identity.clone(),
        extension_id: extension_id.to_string(),
        package_digest: digest.clone(),
        grant_generation: 9,
    };
    {
        let mut state = registry();
        state
            .by_surface
            .insert(first_key.clone(), first_label.clone());
        state.by_label.insert(
            first_label.clone(),
            NativeRecord {
                key: first_key,
                label: first_label.clone(),
                lease: first_lease.to_string(),
                wrapper_url: "http://127.0.0.1:41000/frame/first".to_string(),
                data_directory: std::env::temp_dir().join(&first_label),
                state: NativeWindowState::Open,
                error: None,
                stream_sink: None,
            },
        );
    }
    assert_eq!(
        close_for_identity_extension(app.handle(), &identity, extension_id),
        1
    );
    assert!(registry().by_label.is_empty());
    assert!(super::super::frame_host::lease_authority_snapshot(first_lease).is_none());
    assert_eq!(
        close_for_identity_extension(app.handle(), &identity, extension_id),
        0
    );

    let second_label = "extension-secure-lifecycle-second".to_string();
    assert_ne!(first_label, second_label);
    let second_lease = "55555555-5555-4555-8555-555555555555";
    super::super::frame_host::insert_authorized_lease_with_generation_for_test(
        second_lease,
        extension_id,
        &identity,
        &digest,
        10,
    );
    let second_key = SurfaceKey {
        identity_pubkey: identity.clone(),
        extension_id: extension_id.to_string(),
        package_digest: digest,
        grant_generation: 10,
    };
    {
        let mut state = registry();
        state
            .by_surface
            .insert(second_key.clone(), second_label.clone());
        state.by_label.insert(
            second_label.clone(),
            NativeRecord {
                key: second_key,
                label: second_label.clone(),
                lease: second_lease.to_string(),
                wrapper_url: "http://127.0.0.1:42000/frame/second".to_string(),
                data_directory: std::env::temp_dir().join(&second_label),
                state: NativeWindowState::Opening,
                error: None,
                stream_sink: None,
            },
        );
    }
    assert_eq!(close_for_extension(app.handle(), extension_id), 1);
    assert!(registry().by_label.is_empty());
    assert!(super::super::frame_host::lease_authority_snapshot(second_lease).is_none());
}

#[tokio::test]
async fn two_native_stream_sinks_receive_only_their_exact_lease() {
    let _guard = super::super::frame_host::lifecycle_guard().await;
    let app = tauri::test::mock_app();
    close_all(app.handle());
    let identity = "11".repeat(32);
    let digest_a = "aa".repeat(32);
    let digest_b = "bb".repeat(32);
    let authority_a = authority(&identity, &digest_a, 7);
    let authority_b = authority(&identity, &digest_b, 8);
    let label_a = "extension-secure-stream-a";
    let label_b = "extension-secure-stream-b";
    let lease_a = "11111111-1111-4111-8111-111111111111";
    let lease_b = "22222222-2222-4222-8222-222222222222";
    let url_a = "http://127.0.0.1:41001/frame/context-a/digest/equation-explorer";
    let url_b = "http://127.0.0.1:41002/frame/context-b/digest/equation-explorer";
    super::super::frame_host::insert_authorized_lease_with_label_for_test(
        lease_a,
        "equation-explorer",
        &identity,
        &digest_a,
        7,
        label_a,
        super::super::frame_authority::WrapperMode::WindowsTopLevel,
    );
    super::super::frame_host::insert_authorized_lease_with_label_for_test(
        lease_b,
        "equation-explorer",
        &identity,
        &digest_b,
        8,
        label_b,
        super::super::frame_authority::WrapperMode::WindowsTopLevel,
    );
    insert_record(
        &authority_a,
        label_a,
        lease_a,
        url_a,
        std::env::temp_dir().join(label_a),
        NativeWindowState::Opening,
    );
    insert_record(
        &authority_b,
        label_b,
        lease_b,
        url_b,
        std::env::temp_dir().join(label_b),
        NativeWindowState::Opening,
    );

    let seen_a = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen_b = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let capture_a = std::sync::Arc::clone(&seen_a);
    let capture_b = std::sync::Arc::clone(&seen_b);
    let sink_a = guarded_stream_sink(label_a, lease_a, url_a, move |batch| {
        capture_a.lock().unwrap().push(batch.generation.clone());
        Ok(())
    });
    let sink_b = guarded_stream_sink(label_b, lease_b, url_b, move |batch| {
        capture_b.lock().unwrap().push(batch.generation.clone());
        Ok(())
    });
    assert!(bind_stream_sink(
        label_a,
        lease_a,
        "http://127.0.0.1:9/frame/wrong",
        &authority_a,
        std::sync::Arc::clone(&sink_a)
    )
    .is_err());
    assert!(bind_stream_sink(
        "extension-secure-wrong-label",
        lease_a,
        url_a,
        &authority_a,
        std::sync::Arc::clone(&sink_a)
    )
    .is_err());
    let stale_authority = authority(&identity, &digest_a, 6);
    assert!(bind_stream_sink(
        label_a,
        lease_a,
        url_a,
        &stale_authority,
        std::sync::Arc::clone(&sink_a)
    )
    .is_err());
    bind_stream_sink(label_a, lease_a, url_a, &authority_a, sink_a).unwrap();
    bind_stream_sink(label_b, lease_b, url_b, &authority_b, sink_b).unwrap();
    assert!(bind_stream_sink(
        label_a,
        lease_a,
        url_a,
        &authority_a,
        guarded_stream_sink(label_a, lease_a, url_a, |_| Ok(()))
    )
    .is_err());

    let live_a = stream_sink_for_lease(lease_a).unwrap();
    let live_b = stream_sink_for_lease(lease_b).unwrap();
    assert!(live_a(&batch(lease_a, "sub-a")).is_ok());
    assert!(live_b(&batch(lease_b, "sub-b")).is_ok());
    assert!(live_a(&batch(lease_b, "cross-b-into-a")).is_err());
    assert!(live_b(&batch(lease_a, "cross-a-into-b")).is_err());
    assert_eq!(&*seen_a.lock().unwrap(), &[lease_a.to_string()]);
    assert_eq!(&*seen_b.lock().unwrap(), &[lease_b.to_string()]);

    cleanup_record(
        app.handle(),
        label_a,
        false,
        NativeWindowState::Closed,
        None,
    );
    assert!(live_a(&batch(lease_a, "after-release")).is_err());
    assert_eq!(&*seen_a.lock().unwrap(), &[lease_a.to_string()]);
    assert!(live_b(&batch(lease_b, "still-b")).is_ok());
    cleanup_record(
        app.handle(),
        label_b,
        false,
        NativeWindowState::Closed,
        None,
    );
    assert!(registry().by_label.is_empty());
}

#[tokio::test]
async fn readiness_rejects_wrong_origin_port_path_label_and_stale_lease() {
    let _guard = super::super::frame_host::lifecycle_guard().await;
    let app = tauri::test::mock_app();
    let identity = "33".repeat(32);
    let digest = "cc".repeat(32);
    let authority = authority(&identity, &digest, 11);
    let label = "extension-secure-ready";
    let lease = "33333333-3333-4333-8333-333333333333";
    let url = "http://127.0.0.1:42001/frame/context/digest/equation-explorer";
    super::super::frame_host::insert_authorized_lease_with_label_for_test(
        lease,
        "equation-explorer",
        &identity,
        &digest,
        11,
        label,
        super::super::frame_authority::WrapperMode::WindowsTopLevel,
    );
    insert_record(
        &authority,
        label,
        lease,
        url,
        std::env::temp_dir().join(label),
        NativeWindowState::Opening,
    );
    let sink = guarded_stream_sink(label, lease, url, |_| Ok(()));
    bind_stream_sink(label, lease, url, &authority, sink).unwrap();

    for (candidate_label, candidate_url, candidate_lease) in [
        (
            label,
            "http://127.0.0.1:42002/frame/context/digest/equation-explorer",
            lease,
        ),
        (
            label,
            "http://127.0.0.1:42001/frame/other/digest/equation-explorer",
            lease,
        ),
        (
            label,
            "http://localhost:42001/frame/context/digest/equation-explorer",
            lease,
        ),
        ("extension-secure-other", url, lease),
        (label, url, "44444444-4444-4444-8444-444444444444"),
    ] {
        assert!(super::super::bridge::native_ready_for_caller(
            app.handle(),
            candidate_label,
            candidate_url,
            candidate_lease,
        )
        .is_err());
        assert_eq!(
            registry().by_label.get(label).unwrap().state,
            NativeWindowState::Opening
        );
    }

    cleanup_record(app.handle(), label, false, NativeWindowState::Closed, None);
    assert!(
        super::super::bridge::native_ready_for_caller(app.handle(), label, url, lease).is_err()
    );
    assert!(registry().by_label.is_empty());
}

#[tokio::test]
async fn ready_watchdog_boundary_is_atomic() {
    let _guard = super::super::frame_host::lifecycle_guard().await;
    let app = tauri::test::mock_app();
    let identity = "44".repeat(32);
    let digest = "dd".repeat(32);
    let authority = authority(&identity, &digest, 12);
    let label = "extension-secure-watchdog";
    let lease = "55555555-5555-4555-8555-555555555555";
    let url = "http://127.0.0.1:43001/frame/context/digest/equation-explorer";
    super::super::frame_host::insert_authorized_lease_with_label_for_test(
        lease,
        "equation-explorer",
        &identity,
        &digest,
        12,
        label,
        super::super::frame_authority::WrapperMode::WindowsTopLevel,
    );
    insert_record(
        &authority,
        label,
        lease,
        url,
        std::env::temp_dir().join(label),
        NativeWindowState::Opening,
    );
    let sink = guarded_stream_sink(label, lease, url, |_| Ok(()));
    bind_stream_sink(label, lease, url, &authority, sink).unwrap();
    assert_eq!(
        transition_to_open(label, lease).unwrap().state,
        NativeWindowState::Open
    );
    assert!(cleanup_record_if_state(
        app.handle(),
        label,
        NativeWindowState::Opening,
        false,
        NativeWindowState::Failed,
        Some("stale watchdog".to_string()),
    )
    .is_none());
    assert_eq!(
        registry().by_label.get(label).unwrap().state,
        NativeWindowState::Open
    );
    cleanup_record(app.handle(), label, false, NativeWindowState::Closed, None);
}

#[tokio::test]
async fn close_waits_for_paused_open_then_reaps_every_authority() {
    let _guard = super::super::frame_host::lifecycle_guard().await;
    let app = tauri::test::mock_app();
    let root = tempfile::tempdir().unwrap();
    let package = root.path().join("equation-explorer");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(package.join("extension.json"), br#"{"id":"equation-explorer","name":"Equation Explorer","version":"1","entry":"index.html"}"#).unwrap();
    std::fs::write(package.join("index.html"), "<!doctype html>").unwrap();
    let identity = "55".repeat(32);
    let digest = "ee".repeat(32);
    let label = "extension-secure-paused-open".to_string();
    let udf = root.path().join("udf-paused-open");
    let (paused_tx, paused_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    let open_identity = identity.clone();
    let open_digest = digest.clone();
    let open_label = label.clone();
    let open_udf = udf.clone();
    let open_root = root.path().to_path_buf();
    let open_task = tokio::spawn(async move {
        let _serial = NATIVE_OPEN_LOCK.lock().await;
        let claim = super::super::frame_host::acquire_authorized_with_generation_and_label(
            open_root,
            "equation-explorer",
            &open_identity,
            &open_digest,
            13,
            "index.html",
            Vec::new(),
            &open_label,
            super::super::frame_authority::WrapperMode::WindowsTopLevel,
        )
        .await
        .unwrap();
        std::fs::create_dir_all(&open_udf).unwrap();
        paused_tx.send(()).unwrap();
        resume_rx.await.unwrap();
        let authority = authority(&open_identity, &open_digest, 13);
        insert_record(
            &authority,
            &open_label,
            &claim.lease,
            "http://127.0.0.1:44001/frame/context/digest/equation-explorer",
            open_udf,
            NativeWindowState::Opening,
        );
        (claim.lease, claim.static_context)
    });
    paused_rx.await.unwrap();
    let close_app = app.handle().clone();
    let close_identity = identity.clone();
    let mut close_task = tokio::spawn(async move {
        close_native_extension_window_serialized(&close_app, "equation-explorer", || {
            Ok(close_identity)
        })
        .await
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), &mut close_task)
            .await
            .is_err(),
        "close must wait behind the in-flight open"
    );
    resume_tx.send(()).unwrap();
    let (lease, context) = open_task.await.unwrap();
    assert_eq!(
        close_task.await.unwrap().unwrap().state,
        NativeWindowState::Closed
    );
    assert!(registry().by_label.is_empty());
    assert!(registry().by_surface.is_empty());
    assert!(app.get_webview_window(&label).is_none());
    assert!(super::super::frame_host::lease_authority_snapshot(&lease).is_none());
    assert!(
        super::super::frame_authority::static_owner(&context, &digest, "equation-explorer")
            .is_none()
    );
    assert!(super::super::frame_host::running_port().is_none());
    assert_eq!(super::super::query::live_subscription_count_for_test(), 0);
    assert_eq!(
        super::super::agent_conversation::live_admission_count_for_test(),
        0
    );
    for _ in 0..50 {
        if !udf.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(!udf.exists());
    assert_eq!(
        close_native_extension_window_serialized(app.handle(), "equation-explorer", || {
            Ok(identity)
        })
        .await
        .unwrap()
        .state,
        NativeWindowState::Closed
    );
}

#[tokio::test]
async fn windows_mode_rejects_legacy_iframe_before_any_authority_exists() {
    let _guard = super::super::frame_host::lifecycle_guard().await;
    let app = tauri::test::mock_app();
    let before = {
        let state = super::super::frame_host::host_state();
        (
            state.leases.len(),
            state.contexts.len(),
            state.running.is_some(),
        )
    };
    let result = super::super::open_extension_frame_for_mode(
        app.handle().clone(),
        "equation-explorer".to_string(),
        ExtensionSurfaceMode::WindowsNativeWindow,
    )
    .await;
    assert_eq!(
        result.unwrap_err(),
        "legacy extension frames are unavailable on Windows"
    );
    let after = {
        let state = super::super::frame_host::host_state();
        (
            state.leases.len(),
            state.contexts.len(),
            state.running.is_some(),
        )
    };
    assert_eq!(after, before);
    assert!(registry().by_label.is_empty());
    assert_eq!(
        ExtensionSurfaceMode::LinuxIframe,
        ExtensionSurfaceMode::LinuxIframe
    );
}

#[test]
fn native_capability_is_windows_remote_wrapper_only() {
    let capability: serde_json::Value = serde_json::from_str(include_str!(
        "../../capabilities/extension-native-bridge.json"
    ))
    .unwrap();
    assert_eq!(capability["local"], false);
    assert_eq!(
        capability["windows"],
        serde_json::json!(["extension-secure-*"])
    );
    assert_eq!(
        capability["remote"]["urls"],
        serde_json::json!(["http://127.0.0.1:*/frame/*"])
    );
    assert_eq!(capability["platforms"], serde_json::json!(["windows"]));
    let permissions = capability["permissions"].as_array().unwrap();
    assert_eq!(permissions.len(), 4);
    assert!(permissions.iter().all(|permission| permission
        .as_str()
        .is_some_and(|value| value.starts_with("extension-bridge:"))));
    assert!(!capability.to_string().contains("core:event"));
    let wrapper = include_str!("native_wrapper.js");
    assert!(!wrapper.contains("plugin:event|"));
    assert!(!wrapper.contains("__TAURI_EVENT_PLUGIN_INTERNALS__"));
    assert!(wrapper.contains("plugin:extension-bridge|native_stream_bind"));
    assert!(!capability.to_string().contains("/ext/*"));
    assert!(!capability.to_string().contains("*://*"));
}
