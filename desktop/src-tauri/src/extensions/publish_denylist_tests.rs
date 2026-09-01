//! The never-grantable denylist, against an independent D-2a transcription.

use super::publish_test_support::*;
use super::*;
use crate::extensions::manifest::EXTENSION_SIGNABLE_KINDS;

/// An independent transcription of the never-grantable denylist.
///
/// Written from `docs/DESIGN_AUDIT.md` D-2a and BRIDGE_SPEC §4 check 1 — **not**
/// from the implementation, and deliberately in integer literals rather than
/// `kind::KIND_*` constants. Sharing the constants would make both sides move
/// together if one were renumbered, which is the failure this oracle exists to
/// catch. Every entry cites the D-2a bullet it comes from.
///
/// If the spec changes, this changes first and the implementation follows.
fn spec_never_grantable(kind_value: u32) -> bool {
    matches!(
        kind_value,
        // §4 check 1: "relay-only (`is_relay_only_kind`)" — the six kinds only
        // the relay may author.
        13534 | 40901 | 40902 | 30622 | 39005 | 39006
        // D-2a "Deploy / workflow / approval".
        | 30620 | 46020 | 46030 | 46031
        // D-2a "Membership / group admin" — NIP-29, relay admin, DM membership.
        | 9000 | 9001 | 9002 | 9005 | 9007 | 9008 | 9009
        | 9030 | 9031 | 9032 | 9033
        | 41010 | 41011 | 41012
        // D-2a "Moderation" — 9040–9044.
        | 9040 | 9041 | 9042 | 9043 | 9044
        // D-2a "Identity archival".
        | 9035 | 9036
        // D-2a "Auth / bearer-credential".
        | 22242 | 24242 | 27235 | 24243
        // D-2a "Agent control".
        | 24200
        // D-2a "Git push / ref authority".
        | 30617 | 30618 | 1631 | 1632
        // D-2a "Deletion".
        | 5
    )
}

#[test]
fn the_denylist_matches_the_decision_exactly_in_both_directions() {
    // Sweeping the whole kind space catches a denylist that is *narrower* than
    // the decision (a hole an extension could sign through) and one that is
    // *wider* (a policy nobody wrote down, which is how a list stops being
    // auditable). `manifest_tests.rs` carries the same shape for §5's read
    // floor, after an earlier revision asserted the implementation against
    // itself.
    let mut denied = 0usize;
    for kind_value in 0..=50_000u32 {
        assert_eq!(
            is_never_grantable_kind(kind_value),
            spec_never_grantable(kind_value),
            "kind {kind_value}: denylist disagrees with D-2a / BRIDGE_SPEC §4"
        );
        if spec_never_grantable(kind_value) {
            denied += 1;
        }
    }
    // Pins the oracle's own size. The sweep above proves implementation and
    // oracle agree; this catches an edit that removes an arm from *both* and
    // would therefore still sweep clean.
    //
    // 6 relay-only + 4 deploy/workflow/approval + 14 membership/group-admin
    // (7 NIP-29 + 4 relay-admin + 3 DM) + 5 moderation + 2 identity-archival
    // + 4 auth/bearer + 1 agent-control + 4 git + 1 deletion.
    assert_eq!(
        denied, 41,
        "the oracle enumerates 41 never-grantable kinds; a change here is a spec change"
    );
}

#[test]
fn the_denylist_is_sourced_from_buzz_core_not_copied() {
    // The point of borrowing buzz-core's predicates is that a kind
    // reclassified there is reclassified here without anyone remembering. If
    // someone replaces the predicate calls with an inline list, these stop
    // agreeing — buzz-core stays the single writer for these families.
    for kind_value in 0..=50_000u32 {
        if kind::is_relay_only_kind(kind_value)
            || kind::is_command_kind(kind_value)
            || kind::is_relay_admin_kind(kind_value)
            || kind::is_moderation_command_kind(kind_value)
            || kind::is_identity_archive_request_kind(kind_value)
        {
            assert!(
                is_never_grantable_kind(kind_value),
                "kind {kind_value} is classified as authority-bearing by buzz-core \
                 but the signer denylist does not refuse it"
            );
        }
    }
}

#[test]
fn the_allowlist_and_the_denylist_do_not_overlap() {
    // A kind in both lists would mean the spec contradicts itself, and the
    // ordering of the two gates would become load-bearing by accident.
    for kind_value in EXTENSION_SIGNABLE_KINDS {
        assert!(
            !is_never_grantable_kind(*kind_value),
            "kind {kind_value} is both signable and never-grantable"
        );
    }
}

#[test]
fn the_denylist_refuses_before_the_allowlist_is_consulted() {
    // §4 check 1 is defence in depth, and the gates are ordered so the
    // allowlist would refuse these anyway. That redundancy is what makes a
    // "was it denied?" assertion worthless here: deleting the denylist leaves
    // such a test green, because the next gate catches the same case.
    //
    // Naming the refusing gate is what makes check 1 independently defended —
    // remove it and these become `NotAllowlisted`, and this fails.
    //
    // 9000 (add-member) is the D-2a escape decision 003 names: a grant to sign
    // in channel X plus a tag redirect would otherwise be a takeover of Y.
    for kind_value in [9000u32, 46020, 22242, 5, 9040, 30617] {
        let event = CanonicalEvent {
            kind: kind_value,
            ..message(vec![tag(&["h", CHANNEL])], "hello")
        };
        assert_eq!(
            refusal_with_everything_granted(&event),
            Some(Refusal::NeverGrantable),
            "kind {kind_value} must be stopped by the denylist, not merely by the allowlist"
        );
    }
}

#[test]
fn no_allowlisted_kind_is_shadowed_by_the_denylist() {
    // The mirror direction: the denylist must not be quietly refusing kinds the
    // spec says are signable. Every allowlisted kind reaches at least as far as
    // the grant check.
    for kind_value in EXTENSION_SIGNABLE_KINDS {
        let event = CanonicalEvent {
            kind: *kind_value,
            ..message(vec![tag(&["h", CHANNEL])], "hello")
        };
        assert_ne!(
            refusal_with_everything_granted(&event),
            Some(Refusal::NeverGrantable),
            "kind {kind_value} is allowlisted but the denylist refuses it"
        );
    }
}
