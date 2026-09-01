//! P5 identity/digest-bound frame, bridge and static-host authority.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LeaseAuthority {
    pub(crate) extension_id: String,
    pub(crate) identity_pubkey: String,
    pub(crate) package_digest: String,
}

#[derive(Debug, Clone)]
pub(super) struct LeaseOwner {
    pub(super) authority: LeaseAuthority,
    /// A separate opaque route capability for this one frame. It is never
    /// derived from the extension id and is removed with the lease.
    pub(super) static_context: String,
    /// Frozen from the validated installed manifest at open time. Wrapper
    /// requests never re-read a mutable package manifest.
    pub(super) entry: String,
    pub(super) egress: Vec<String>,
}

pub(super) fn content_security_policy_with_egress(origin: &str, egress: &[String]) -> String {
    let connect_src = if egress.is_empty() {
        "'none'".to_string()
    } else {
        egress.join(" ")
    };
    format!(
        "default-src 'none'; \
         script-src {origin}; \
         style-src {origin} 'unsafe-inline'; \
         img-src {origin} data: blob:; \
         font-src {origin}; \
         media-src {origin}; \
         connect-src {connect_src}; \
         webrtc 'block'; \
         base-uri 'none'; \
         form-action 'none'"
    )
}

/// Resolve one opaque static-host context to exactly one still-live lease owner.
/// There is no extension-id scan and therefore no insertion-order authority.
pub(super) fn static_owner(
    context: &str,
    package_digest: &str,
    extension_id: &str,
) -> Option<LeaseOwner> {
    let state = super::frame_host::host_state();
    let lease = state.contexts.get(context)?;
    let owner = state.leases.get(lease)?;
    (owner.static_context == context
        && owner.authority.package_digest == package_digest
        && owner.authority.extension_id == extension_id)
        .then(|| owner.clone())
}

pub(crate) fn lease_authority_snapshot(lease: &str) -> Option<LeaseAuthority> {
    super::frame_host::host_state()
        .leases
        .get(lease)
        .map(|owner| owner.authority.clone())
}

pub(crate) fn extension_for_lease(lease: &str) -> Option<String> {
    lease_authority_snapshot(lease).map(|owner| owner.extension_id)
}

pub(crate) fn lease_authority(lease: &str) -> Option<(String, String, String)> {
    lease_authority_snapshot(lease).map(|owner| {
        (
            owner.extension_id,
            owner.identity_pubkey,
            owner.package_digest,
        )
    })
}

pub(crate) fn release_for_identity_extension(identity_pubkey: &str, extension_id: &str) -> usize {
    let leases: Vec<String> = super::frame_host::host_state()
        .leases
        .iter()
        .filter(|(_, owner)| {
            owner.authority.identity_pubkey == identity_pubkey
                && owner.authority.extension_id == extension_id
        })
        .map(|(lease, _)| lease.clone())
        .collect();
    for lease in &leases {
        super::frame_host::release(lease);
    }
    leases.len()
}

/// Fence every current and in-flight open for an extension, not only leases
/// visible at the instant of the sweep.
pub(crate) fn release_for_extension_id(extension_id: &str) -> usize {
    super::frame_host::fence_extension(extension_id)
}

#[cfg(test)]
pub(crate) fn insert_authorized_lease_for_test(
    lease: &str,
    extension_id: &str,
    identity_pubkey: &str,
    package_digest: &str,
) {
    let context = format!("test-context-{lease}");
    let mut state = super::frame_host::host_state();
    state.contexts.insert(context.clone(), lease.to_string());
    state.leases.insert(
        lease.to_string(),
        LeaseOwner {
            authority: LeaseAuthority {
                extension_id: extension_id.to_string(),
                identity_pubkey: identity_pubkey.to_string(),
                package_digest: package_digest.to_string(),
            },
            static_context: context,
            entry: "index.html".to_string(),
            egress: Vec::new(),
        },
    );
}

#[cfg(test)]
pub(crate) static LIFECYCLE_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
pub(crate) async fn lifecycle_guard() -> tokio::sync::MutexGuard<'static, ()> {
    let guard = LIFECYCLE_TEST_LOCK.lock().await;
    super::frame_host::shutdown_now();
    guard
}

#[cfg(test)]
pub(crate) fn insert_lease_for_test(lease: &str, extension_id: &str) {
    insert_authorized_lease_for_test(lease, extension_id, "", "");
}

#[cfg(test)]
pub(crate) fn running_port() -> Option<u16> {
    super::frame_host::host_state()
        .running
        .as_ref()
        .map(|running| running.extension_port)
}
