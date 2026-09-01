//! Frame listener and exact lease lifecycle.

use super::*;

/// Start the host if it is not running and issue a lease to one live frame.
///
/// Idempotent in the sense that matters: a second frame reuses the running
/// listener, but gets its **own** lease.
#[cfg(test)]
pub(crate) async fn acquire(base_dir: PathBuf, extension_id: &str) -> Result<FrameLease, String> {
    acquire_inner(
        base_dir,
        extension_id,
        "test-identity",
        "test-digest",
        "index.html",
        Vec::new(),
    )
    .await
}

pub(crate) async fn acquire_authorized(
    base_dir: PathBuf,
    extension_id: &str,
    identity_pubkey: &str,
    package_digest: &str,
    entry: &str,
    egress: Vec<String>,
) -> Result<FrameLease, String> {
    if identity_pubkey.is_empty() || package_digest.is_empty() || entry.is_empty() {
        return Err("enabled extension authority is incomplete".to_string());
    }
    acquire_inner(
        base_dir,
        extension_id,
        identity_pubkey,
        package_digest,
        entry,
        egress,
    )
    .await
}

fn install_owner(state: &mut FrameHostState, lease: &str, owner: &LeaseOwner) {
    state
        .contexts
        .insert(owner.static_context.clone(), lease.to_string());
    state.leases.insert(lease.to_string(), owner.clone());
}

async fn acquire_inner(
    base_dir: PathBuf,
    extension_id: &str,
    identity_pubkey: &str,
    package_digest: &str,
    entry: &str,
    egress: Vec<String>,
) -> Result<FrameLease, String> {
    let lease = uuid::Uuid::new_v4().to_string();
    let static_context = uuid::Uuid::new_v4().to_string();
    let owner = LeaseOwner {
        authority: LeaseAuthority {
            extension_id: extension_id.to_string(),
            identity_pubkey: identity_pubkey.to_string(),
            package_digest: package_digest.to_string(),
        },
        static_context: static_context.clone(),
        entry: entry.to_string(),
        egress,
    };
    let (opening_epoch, opening_extension_epoch) = {
        let mut state = host_state();
        if let Some(running) = &state.running {
            let (extension_port, wrapper_port) = (running.extension_port, running.wrapper_port);
            install_owner(&mut state, &lease, &owner);
            return Ok(FrameLease {
                extension_port,
                wrapper_port,
                lease,
                static_context,
                package_digest: package_digest.to_string(),
            });
        }
        let extension_epoch = *state
            .extension_epochs
            .entry(extension_id.to_string())
            .or_default();
        (state.epoch, extension_epoch)
    };

    // Bind outside the lock: these are the only awaits in the path.
    let extension_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|error| format!("could not start the extension frame host: {error}"))?;
    let extension_port = extension_listener
        .local_addr()
        .map_err(|error| format!("could not read the frame host address: {error}"))?
        .port();
    let wrapper_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|error| format!("could not start the extension wrapper host: {error}"))?;
    let wrapper_port = wrapper_listener
        .local_addr()
        .map_err(|error| format!("could not read the wrapper host address: {error}"))?
        .port();

    let (shutdown_extension, extension_rx) = oneshot::channel();
    let (shutdown_wrapper, wrapper_rx) = oneshot::channel();
    let extension_router = build_extension_router(base_dir.clone(), extension_port);
    let wrapper_router = build_wrapper_router(base_dir, extension_port);
    tokio::spawn(async move {
        axum::serve(extension_listener, extension_router)
            .with_graceful_shutdown(async move {
                extension_rx.await.ok();
            })
            .await
            .ok();
    });
    tokio::spawn(async move {
        axum::serve(wrapper_listener, wrapper_router)
            .with_graceful_shutdown(async move {
                wrapper_rx.await.ok();
            })
            .await
            .ok();
    });

    #[cfg(test)]
    frame_host_test_support::pause_before_install().await;

    let mut state = host_state();
    let current_extension_epoch = *state
        .extension_epochs
        .entry(extension_id.to_string())
        .or_default();
    if state.epoch != opening_epoch || current_extension_epoch != opening_extension_epoch {
        let _ = shutdown_extension.send(());
        let _ = shutdown_wrapper.send(());
        return Err("the extension frame closed while its host was opening".to_string());
    }
    if let Some(running) = &state.running {
        let (extension_port, wrapper_port) = (running.extension_port, running.wrapper_port);
        let _ = shutdown_extension.send(());
        let _ = shutdown_wrapper.send(());
        install_owner(&mut state, &lease, &owner);
        return Ok(FrameLease {
            extension_port,
            wrapper_port,
            lease,
            static_context,
            package_digest: package_digest.to_string(),
        });
    }
    state.running = Some(RunningHost {
        extension_port,
        wrapper_port,
        shutdown_extension,
        shutdown_wrapper,
    });
    install_owner(&mut state, &lease, &owner);
    Ok(FrameLease {
        extension_port,
        wrapper_port,
        lease,
        static_context,
        package_digest: package_digest.to_string(),
    })
}

/// Release one lease, stopping the host when the last one goes.
///
/// Idempotent and unforgeable-ish: releasing a lease that was never issued, or
/// releasing the same one twice, does nothing. A frame that failed to open has
/// no lease to present, so its cleanup is a no-op instead of a theft.
pub(crate) fn release(lease: &str) {
    let released = {
        let mut state = host_state();
        let Some(owner) = state.leases.remove(lease) else {
            return;
        };
        state.contexts.remove(&owner.static_context);
        if state.leases.is_empty() {
            state.epoch = state.epoch.wrapping_add(1);
            if let Some(running) = state.running.take() {
                let _ = running.shutdown_extension.send(());
                let _ = running.shutdown_wrapper.send(());
            }
        }
        true
    };
    if released {
        super::super::query::close_subscriptions_for_lease(lease);
    }
}

/// Advance the exact extension generation and remove every current owner in a
/// single sweep. A paused bind holding the predecessor generation loses when it
/// tries to install, even if the sweep observed no completed lease.
pub(crate) fn fence_extension(extension_id: &str) -> usize {
    let leases = {
        let mut state = host_state();
        let generation = state
            .extension_epochs
            .entry(extension_id.to_string())
            .or_default();
        *generation = generation.wrapping_add(1);
        let leases: Vec<String> = state
            .leases
            .iter()
            .filter(|(_, owner)| owner.authority.extension_id == extension_id)
            .map(|(lease, _)| lease.clone())
            .collect();
        for lease in &leases {
            if let Some(owner) = state.leases.remove(lease) {
                state.contexts.remove(&owner.static_context);
            }
        }
        if !leases.is_empty() && state.leases.is_empty() {
            state.epoch = state.epoch.wrapping_add(1);
            if let Some(running) = state.running.take() {
                let _ = running.shutdown_extension.send(());
                let _ = running.shutdown_wrapper.send(());
            }
        }
        leases
    };
    for lease in &leases {
        super::super::query::close_subscriptions_for_lease(lease);
    }
    leases.len()
}

/// Stop the host unconditionally, whatever the holder count says.
///
/// Called on app shutdown. A frontend that never released — a crashed webview,
/// a reload — must not leave a listener behind the process.
pub(crate) fn shutdown_now() {
    let leases = {
        let mut state = host_state();
        state.epoch = state.epoch.wrapping_add(1);
        let leases: Vec<String> = state.leases.keys().cloned().collect();
        state.leases.clear();
        state.contexts.clear();
        if let Some(running) = state.running.take() {
            let _ = running.shutdown_extension.send(());
            let _ = running.shutdown_wrapper.send(());
        }
        leases
    };
    for lease in leases {
        super::super::query::close_subscriptions_for_lease(&lease);
    }
}
