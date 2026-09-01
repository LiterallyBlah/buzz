//! Fixtures shared by the two extension-data test modules.
//!
//! `extension_data_tests` proves §4's method behaviour; `extension_data_authority_tests`
//! proves the authority transitions around the admission gate. They share an
//! extension id, key and lease, so those live here rather than in either module
//! — two copies of a constant is how one of them silently stops matching the
//! grant the other registered.

use super::*;

pub(super) const EXTID: &str = "demo";
pub(super) const KEY: &str = "graph.v1";
/// A host-minted lease, registered in the real frame-host map by the tests that
/// need the production lease check to resolve.
pub(super) const LEASE: &str = "lease-for-extension-data-tests";

pub(super) fn code_of(reply: &BridgeReply) -> Option<&str> {
    reply.error.as_ref().map(|e| e.code.as_str())
}

pub(super) fn denied(reply: &BridgeReply) -> bool {
    code_of(reply) == Some(code::DENIED)
}
