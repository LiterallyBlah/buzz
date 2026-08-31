//! P5 identity/digest-bound frame authority and selected egress policy.

use std::path::Path;

#[derive(Debug, Clone)]
pub(super) struct LeaseOwner {
    pub(super) extension_id: String,
    pub(super) identity_pubkey: String,
    pub(super) package_digest: String,
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

fn active_owner(extension_id: &str) -> Option<LeaseOwner> {
    super::frame_host::host_state()
        .leases
        .values()
        .find(|owner| owner.extension_id == extension_id)
        .cloned()
}

pub(super) fn active_owner_for_tree(base_dir: &Path, extension_id: &str) -> Option<LeaseOwner> {
    let owner = active_owner(extension_id)?;
    if owner.package_digest.is_empty() {
        return Some(owner);
    }
    let digest = super::management::package_digest(&base_dir.join(extension_id)).ok()?;
    (digest == owner.package_digest).then_some(owner)
}

pub(crate) fn extension_for_lease(lease: &str) -> Option<String> {
    super::frame_host::host_state()
        .leases
        .get(lease)
        .map(|owner| owner.extension_id.clone())
}

pub(crate) fn lease_authority(lease: &str) -> Option<(String, String, String)> {
    super::frame_host::host_state()
        .leases
        .get(lease)
        .map(|owner| {
            (
                owner.extension_id.clone(),
                owner.identity_pubkey.clone(),
                owner.package_digest.clone(),
            )
        })
}

pub(crate) fn release_for_identity_extension(identity_pubkey: &str, extension_id: &str) -> usize {
    let leases: Vec<String> = super::frame_host::host_state()
        .leases
        .iter()
        .filter(|(_, owner)| {
            owner.identity_pubkey == identity_pubkey && owner.extension_id == extension_id
        })
        .map(|(lease, _)| lease.clone())
        .collect();
    for lease in &leases {
        super::frame_host::release(lease);
    }
    leases.len()
}

pub(crate) fn release_for_extension_id(extension_id: &str) -> usize {
    let leases: Vec<String> = super::frame_host::host_state()
        .leases
        .iter()
        .filter(|(_, owner)| owner.extension_id == extension_id)
        .map(|(lease, _)| lease.clone())
        .collect();
    for lease in &leases {
        super::frame_host::release(lease);
    }
    leases.len()
}

#[cfg(test)]
pub(crate) fn insert_authorized_lease_for_test(
    lease: &str,
    extension_id: &str,
    identity_pubkey: &str,
    package_digest: &str,
) {
    super::frame_host::host_state().leases.insert(
        lease.to_string(),
        LeaseOwner {
            extension_id: extension_id.to_string(),
            identity_pubkey: identity_pubkey.to_string(),
            package_digest: package_digest.to_string(),
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
    super::frame_host::host_state().leases.insert(
        lease.to_string(),
        LeaseOwner {
            extension_id: extension_id.to_string(),
            identity_pubkey: String::new(),
            package_digest: String::new(),
            egress: Vec::new(),
        },
    );
}

#[cfg(test)]
pub(crate) fn running_port() -> Option<u16> {
    super::frame_host::host_state()
        .running
        .as_ref()
        .map(|running| running.extension_port)
}
