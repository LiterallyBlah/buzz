# Decision 004 successor measurement addendum

Date: 2026-09-02

The first complete owner WebView2 result at predecessor harness commit `211038a747097c4165ff5b899b0c3160c1fd5df6` is final: **FAIL**, 9/10 rows passing. The proxy plus `disable_non_proxied_udp` candidate is eliminated because valid STUN and TURN/UDP traffic reached the controlled loopback sink. That result is retained separately and must not be rerun or imported into this successor.

This branch adds only the already-defined successor row:

`extension_environment_webrtc_disable_is_realm_complete_and_scoped`

The candidate is a dedicated extension WebView2 environment with its own user-data folder and Tauri 2.11.5 `initialization_script_for_all_frames`. The exact document-created script neutralises `RTCPeerConnection`, prefixed aliases and data-channel constructor names before package code in every frame. An otherwise-identical dedicated extension environment omits the script, while a third normal huddle environment remains unmodified.

Both extension arms execute in the initial realm and in the known `srcdoc` plus external package-script bypass realm. The candidate-off and huddle controls must reach separate loopback and off-host protocol sinks for STUN/UDP, TURN/UDP, TURN/TCP and TURNS/TLS. TURN usernames carry the per-run attacker token; the huddle data channel carries the same token. An unreachable or nonce-dead off-host control makes the result `VOID`, never `PASS`.

A `PASS` requires all of the following in one first complete run:

1. candidate-off constructor, offer and `setLocalDescription` witnesses are live in both realms;
2. candidate-off and huddle protocol counters are positive at loopback and off-host sinks;
3. the huddle data-channel token is received;
4. the protected arm cannot construct WebRTC in either realm;
5. every protected sink counter remains zero;
6. distinct UDFs, exact browser arguments, injected-script hash, sink/config hashes and source hashes are retained.

The test certificate/private key in `sink/` is a public, non-secret measurement fixture. Every arm explicitly carries `--ignore-certificate-errors` only to keep the self-contained TURNS control live; this does not distinguish protected from candidate-off and is recorded in the result. The sink is bounded, credential-free and writes nonce-bound counters only.

This is **not** a production migration. It does not change Buzz Desktop, Equation Explorer, updater metadata, grants, bridge authority or the default-off Windows prohibition. Linux compilation and unit tests establish source validity only; they do not constitute WebView2 acceptance.
