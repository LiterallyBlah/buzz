//! The sealed filter builder's own rows: what leaves the seal, and in what shape.
//!
//! Split from `query_tests.rs` when that file passed the repo's 1000-line
//! ratchet. These are the rows that exercise `construction.rs` rather than the
//! verifier, so the split follows the module boundary rather than cutting an
//! arbitrary line.

use super::construction::construct_filters;
use super::*;

const CHAN_A: &str = "11111111-1111-4111-8111-111111111111";
const CHAN_B: &str = "22222222-2222-4222-8222-222222222222";

/// A filter the §5 grammar accepts. Panics on a filter it does not, because a
/// rejected fixture would make every row below vacuous.
fn ok_request(filter: serde_json::Value) -> ValidatedRequest {
    validate_request(&serde_json::json!({ "filter": filter })).expect("filter should be accepted")
}

// ── the REQ burst leaves the seal as wire text ─────────────────────────────
//
// `subscribe` opens one relay branch per emitted filter. It cannot build those
// frames itself without the filters, and handing those out is what broke the
// seal once already — so the frames are serialised in here and only text
// leaves.

#[test]
fn a_req_frame_is_built_per_filter_carrying_that_filter() {
    // The positive control, and the shape check: `["REQ", <branch>, <filter>]`
    // with the branch ids paired in order.
    let granted = vec![(9u32, CHAN_A.to_string()), (9u32, CHAN_B.to_string())];
    let filters = construct_filters(&granted, &ok_request(serde_json::json!({}))).expect("build");
    let branches = vec!["branch-1".to_string(), "branch-2".to_string()];

    let frames = filters
        .req_frames(&branches)
        .expect("one branch per filter");
    assert_eq!(frames.len(), 2);
    for (index, text) in frames.iter().enumerate() {
        let parsed: serde_json::Value = serde_json::from_str(text).expect("valid JSON");
        let parts = parsed.as_array().expect("array");
        assert_eq!(parts[0], "REQ");
        assert_eq!(parts[1], branches[index], "branch ids are paired in order");
        let hs = parts[2]["#h"]
            .as_array()
            .expect("the filter travels with it");
        assert_eq!(hs.len(), 1, "still exactly one #h per branch");
    }
}

#[test]
fn a_req_burst_with_the_wrong_number_of_branches_is_refused() {
    // Zipping instead would open fewer branches than the aggregate was built to
    // span, and an aggregate waiting on a branch nobody requested never reaches
    // its public eose — it hangs to the deadline and closes, which reads to an
    // extension as a relay that never answered.
    let granted = vec![(9u32, CHAN_A.to_string()), (9u32, CHAN_B.to_string())];
    let filters = construct_filters(&granted, &ok_request(serde_json::json!({}))).expect("build");

    assert!(
        filters.req_frames(&["only-one".to_string()]).is_none(),
        "too few branches must be refused, not zipped short"
    );
    assert!(
        filters
            .req_frames(&["a".to_string(), "b".to_string(), "c".to_string()])
            .is_none(),
        "and too many, which would leave a branch nobody opened"
    );
    assert!(
        filters
            .req_frames(&["a".to_string(), "b".to_string()])
            .is_some(),
        "the matching count still builds — the refusal is not unconditional"
    );
}

#[test]
fn the_req_burst_spans_exactly_the_filters_the_seal_counted() {
    // `filter_count` is what `subscribe` reserves quota for and sizes the
    // aggregate by. If it disagreed with what `req_frames` emits, the aggregate
    // would span a different set of branches than the burst opened.
    let granted = vec![
        (9u32, CHAN_A.to_string()),
        (9u32, CHAN_B.to_string()),
        (45001u32, CHAN_A.to_string()),
    ];
    let filters = construct_filters(&granted, &ok_request(serde_json::json!({}))).expect("build");
    let branches: Vec<String> = (0..filters.filter_count())
        .map(|n| format!("branch-{n}"))
        .collect();
    assert_eq!(
        filters.req_frames(&branches).expect("built").len(),
        filters.filter_count(),
        "one REQ per counted filter"
    );
}
