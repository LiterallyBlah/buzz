//! NIP-PC: Peer Agent Calls — the parts both runtimes must compute identically.
//!
//! One trusted agent calls another to perform a single bounded task; the callee
//! returns exactly one correlated result to the surface the call came from.
//!
//! This module holds only what a *builder* and a *validator* must agree on
//! byte-for-byte: the route token, the call-id derivation, and the protocol
//! limits. Trust resolution, the outstanding-call ledger and the admission
//! decision live in the harness (`buzz-acp::peer_call`), because they depend on
//! runtime state rather than on the wire.
//!
//! Keeping the derivation here rather than in the builder crate is what lets
//! `buzz-sdk` (which writes calls) and `buzz-acp` (which judges them) share one
//! implementation. Two copies of a hash construction is two chances to disagree,
//! and the disagreement is silent: every call one side writes is simply refused
//! by the other, with a correct-looking "call id mismatch" on the floor.
//!
//! See `docs/nips/NIP-PC.md` for the full specification.

use sha2::{Digest, Sha256};

use crate::kind::{KIND_JOB_REQUEST, KIND_JOB_RESULT};

/// Kind carrying a call envelope (NIP-PC §Event kinds).
pub const KIND_PEER_CALL: u32 = KIND_JOB_REQUEST;

/// Kind carrying a correlated result (NIP-PC §Event kinds).
pub const KIND_PEER_CALL_RESULT: u32 = KIND_JOB_RESULT;

/// Domain separator for the call-id hash.
///
/// Prefixing the version means a future revision of the derivation produces
/// entirely different ids rather than colliding with this one.
pub const CALL_ID_DOMAIN: &str = "buzz/peer-call/v1";

/// Maximum call depth (NIP-PC §Hop count and visited set).
pub const MAX_HOP: u32 = 3;

/// Maximum concurrent outstanding calls per originating route.
pub const MAX_FANOUT: usize = 10;

/// Maximum `content` length for a call or a result, in bytes.
pub const MAX_CALL_CONTENT_BYTES: usize = 16 * 1024;

/// Length of the call nonce in hex characters (16 bytes).
pub const NONCE_HEX_LEN: usize = 32;

/// The conversation surface a call was made from, and returns to.
///
/// Exactly one form is present on any envelope. The two are separate variants
/// rather than a struct with optional fields so that "a channel call that also
/// carries a repo coordinate" is unrepresentable rather than merely invalid —
/// an event carrying both route forms is refused at parse time, and nothing
/// downstream has to decide which one wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerCallRoute {
    /// A channel, optionally within a thread.
    Channel {
        /// Channel UUID, lowercase and hyphenated.
        channel: String,
        /// Thread root event id, when the call was made inside a thread.
        thread_root: Option<String>,
    },
    /// A project issue or pull request.
    Project {
        /// `30617:<owner>:<identifier>` repository coordinate.
        coordinate: String,
        /// Issue or PR root event id.
        root: String,
    },
}

impl PeerCallRoute {
    /// The canonical route token hashed into the call id.
    ///
    /// Distinct prefixes (`channel:` / `project:`) keep the two route spaces
    /// from colliding: without them a channel whose UUID happened to equal a
    /// coordinate would hash the same, and more practically a reader of the
    /// token could not tell which surface it named.
    pub fn route_token(&self) -> String {
        match self {
            PeerCallRoute::Channel {
                channel,
                thread_root,
            } => format!(
                "channel:{}:{}",
                channel.to_ascii_lowercase(),
                thread_root
                    .as_deref()
                    .unwrap_or_default()
                    .to_ascii_lowercase()
            ),
            PeerCallRoute::Project { coordinate, root } => {
                format!("project:{}:{}", coordinate, root.to_ascii_lowercase())
            }
        }
    }
}

/// Derive the canonical call id for `(caller, callee, route, nonce)`.
///
/// The id is *derived, not chosen*, so a captured id cannot be re-signed toward
/// a different callee or onto a different route: the recomputation a receiver
/// performs would no longer match. This is not by itself a replay defence —
/// replaying the identical call to the identical callee still produces the
/// identical id, which is exactly why the harness also keeps a seen-id ledger.
///
/// Inputs are lowercased here rather than assumed lowercase, so a caller that
/// held its pubkey in mixed case derives the same id a validator does.
pub fn derive_call_id(caller: &str, callee: &str, route: &PeerCallRoute, nonce: &str) -> String {
    let mut hasher = Sha256::new();
    for field in [
        CALL_ID_DOMAIN,
        &caller.to_ascii_lowercase(),
        &callee.to_ascii_lowercase(),
        &route.route_token(),
        &nonce.to_ascii_lowercase(),
    ] {
        hasher.update(field.as_bytes());
        hasher.update(b"\n");
    }
    // The trailing separator after the nonce is deliberate and matches the
    // loop: every field is terminated, so no two field splittings can produce
    // the same byte string.
    hex::encode(hasher.finalize())
}

/// The `(hop, visited)` a call from `agent` must carry, given the path it
/// inherited.
///
/// One producer for the depth bookkeeping, shared by the CLI that publishes
/// calls and by the harness that judges them. `hop` is *derived* from the path
/// rather than supplied alongside it, because NIP-PC requires the two to agree
/// and anything a caller can state separately is something it can state wrongly:
/// a hand-written `--hop 1` beside a three-entry path is an envelope every
/// receiver refuses, for a reason the operator cannot see from the command they
/// typed.
///
/// Entries are lowercased and de-duplicated, and `agent` is appended only if the
/// path does not already contain it — an agent that reappears in its own
/// inherited path is a revisit, and the receiver refuses it rather than this
/// silently rewriting history.
pub fn onward_context(inherited: &[String], agent: &str) -> (u32, Vec<String>) {
    let agent = agent.to_ascii_lowercase();
    let mut visited: Vec<String> = Vec::with_capacity(inherited.len() + 1);
    for entry in inherited {
        let entry = entry.to_ascii_lowercase();
        if !visited.contains(&entry) {
            visited.push(entry);
        }
    }
    if !visited.contains(&agent) {
        visited.push(agent);
    }
    (visited.len() as u32, visited)
}

/// Is `s` exactly `len` lowercase hex characters?
///
/// Lowercase is required rather than normalised because these values are hashed
/// and compared as written. Accepting uppercase here would mean two spellings of
/// one call id, and the ledger that refuses replays is a set of strings.
pub fn is_lowercase_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Parse a `hop` tag value under NIP-PC's strict integer rules.
///
/// Rejects a leading `+`, a leading zero, whitespace, and anything outside
/// `1..=MAX_HOP`. `"01"` and `" 1"` are refused rather than read as `1`: a hop
/// count is compared against a visited-set length, and two spellings of one
/// number is the kind of slack that makes such a comparison decorative.
pub fn parse_hop(raw: &str) -> Option<u32> {
    if raw.is_empty() || raw.len() > 2 || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if raw.starts_with('0') {
        return None;
    }
    let hop: u32 = raw.parse().ok()?;
    (1..=MAX_HOP).contains(&hop).then_some(hop)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CALLER: &str = "93941e544971f89d581a19acd4570572f4d5f7bb0783a9ac1febfa1dc0deaebf";
    const CALLEE: &str = "222b9658e0e4945cbca51ffa8d364a178a02e349d79847e9282e6ee1306a00ce";
    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    fn channel_route() -> PeerCallRoute {
        PeerCallRoute::Channel {
            channel: "8f377516-7391-47bf-bcc4-249a1028b212".into(),
            thread_root: None,
        }
    }

    fn project_route() -> PeerCallRoute {
        PeerCallRoute::Project {
            coordinate: format!("30617:{CALLER}:buzz"),
            root: "48be1cc2000000000000000000000000000000000000000000000000000000ab".into(),
        }
    }

    /// The published derivation is a wire contract, so it is pinned to a
    /// literal. A refactor that changes the hash silently breaks every peer
    /// runtime; this test is the thing that makes that loud.
    #[test]
    fn the_call_id_derivation_is_pinned_to_its_published_value() {
        assert_eq!(
            derive_call_id(CALLER, CALLEE, &channel_route(), NONCE),
            "4c18610bc144b683c556f8297c3b5600b0d14c6b1a05c1ade8d62b553932ba64"
        );
    }

    #[test]
    fn a_call_id_does_not_survive_being_pointed_at_another_callee() {
        let mine = derive_call_id(CALLER, CALLEE, &channel_route(), NONCE);
        let theirs = derive_call_id(CALLER, CALLER, &channel_route(), NONCE);
        assert_ne!(mine, theirs);
    }

    #[test]
    fn a_call_id_does_not_survive_being_moved_to_another_route() {
        let in_channel = derive_call_id(CALLER, CALLEE, &channel_route(), NONCE);
        let in_project = derive_call_id(CALLER, CALLEE, &project_route(), NONCE);
        assert_ne!(in_channel, in_project);
    }

    #[test]
    fn a_thread_bound_call_differs_from_the_top_level_call_beside_it() {
        let top_level = derive_call_id(CALLER, CALLEE, &channel_route(), NONCE);
        let threaded = derive_call_id(
            CALLER,
            CALLEE,
            &PeerCallRoute::Channel {
                channel: "8f377516-7391-47bf-bcc4-249a1028b212".into(),
                thread_root: Some(
                    "48be1cc2000000000000000000000000000000000000000000000000000000ab".into(),
                ),
            },
            NONCE,
        );
        assert_ne!(top_level, threaded);
    }

    /// Field separation, not concatenation: moving a character across a field
    /// boundary must not produce the same id.
    #[test]
    fn adjacent_fields_cannot_be_reassociated_into_the_same_id() {
        let a = derive_call_id("ab", "cd", &channel_route(), NONCE);
        let b = derive_call_id("a", "bcd", &channel_route(), NONCE);
        assert_ne!(a, b);
    }

    #[test]
    fn case_differences_in_the_inputs_do_not_change_the_id() {
        assert_eq!(
            derive_call_id(CALLER, CALLEE, &channel_route(), NONCE),
            derive_call_id(
                &CALLER.to_ascii_uppercase(),
                &CALLEE.to_ascii_uppercase(),
                &channel_route(),
                &NONCE.to_ascii_uppercase(),
            )
        );
    }

    #[test]
    fn hop_parsing_refuses_every_spelling_that_is_not_the_number_itself() {
        assert_eq!(parse_hop("1"), Some(1));
        assert_eq!(parse_hop("3"), Some(3));
        for refused in [
            "0", "01", "+1", " 1", "1 ", "", "4", "10", "-1", "one", "1.0",
        ] {
            assert_eq!(parse_hop(refused), None, "{refused} should not parse");
        }
    }

    /// Depth is derived from the path, so the two can never be stated
    /// inconsistently by a caller that uses this.
    #[test]
    fn the_onward_hop_is_the_length_of_the_path_it_describes() {
        let (hop, visited) = onward_context(&[], CALLER);
        assert_eq!(hop, 1);
        assert_eq!(visited, vec![CALLER.to_string()]);

        let (hop, visited) = onward_context(&[CALLER.to_string()], CALLEE);
        assert_eq!(hop, 2);
        assert_eq!(visited, vec![CALLER.to_string(), CALLEE.to_string()]);
        assert_eq!(hop as usize, visited.len());
    }

    #[test]
    fn an_agent_already_in_the_path_is_not_appended_twice() {
        let (hop, visited) = onward_context(&[CALLER.to_string(), CALLEE.to_string()], CALLER);
        assert_eq!(hop, 2);
        assert_eq!(visited, vec![CALLER.to_string(), CALLEE.to_string()]);
    }

    #[test]
    fn an_inherited_path_is_normalised_before_it_is_carried() {
        let (hop, visited) = onward_context(
            &[CALLER.to_ascii_uppercase(), CALLER.to_string()],
            &CALLEE.to_ascii_uppercase(),
        );
        assert_eq!(hop, 2);
        assert_eq!(visited, vec![CALLER.to_string(), CALLEE.to_string()]);
    }

    #[test]
    fn uppercase_hex_is_not_lowercase_hex() {
        assert!(is_lowercase_hex(NONCE, NONCE_HEX_LEN));
        assert!(!is_lowercase_hex(
            &NONCE.to_ascii_uppercase(),
            NONCE_HEX_LEN
        ));
        assert!(!is_lowercase_hex("abcg", 4));
        assert!(!is_lowercase_hex(NONCE, 64));
    }
}
