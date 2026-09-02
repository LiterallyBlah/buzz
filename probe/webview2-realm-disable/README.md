# Windows WebView2 realm-complete disable successor

This is a bounded, owner-gated **measurement harness**, not a Buzz Desktop migration or release. It implements decision 004's single successor row with three WebView2 environments and five report lanes:

- protected dedicated extension UDF: initial + `srcdoc` external-script realm;
- otherwise-identical candidate-off extension UDF: initial + `srcdoc` realm;
- normal huddle UDF with a real local data-channel token exchange.

Tauri is pinned to `2.11.5`, Wry to `0.55.1` and `webview2-com` to `0.38.2` by `Cargo.lock`. The protected environment alone uses `initialization_script_for_all_frames`; Tauri's exact API contract installs it after global creation but before document parsing and package scripts, including subframes on Windows.

## Source gates on Linux

These do not constitute WebView2 acceptance:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
python3 -m unittest sink/test_controlled_sink.py
cargo check --locked --target x86_64-pc-windows-msvc
python3 tools/package.py --output webview2-realm-disable-successor.zip
```

## Later Arachne-authorised operator sequence

Do not run this sequence unless Arachne explicitly authorises the new successor. Never rerun the completed predecessor harness.

### 1. Windows owner prepares one run token

From a fresh extraction in Developer PowerShell for VS 2022:

```powershell
Set-ExecutionPolicy -Scope Process Bypass
.\prepare-run.ps1
```

This creates `sink-request.json` once. Send that file unchanged to the controlled sink operator. Do not paste its token into chat.

### 2. Separate controlled sink host

The sink host needs Python 3 and inbound UDP/TCP reachability only for the bounded run. No service credentials, paid service or Buzz state are used.

```sh
./sink/run-offhost-sink.sh /exact/path/sink-request.json <EXACT_ARACHNE_APPROVED_IP_OR_DNS> /exact/fresh/output-directory
```

The later owner packet must replace both exact paths and the approved address before delivery; Michael must not improvise them. Return `offhost-endpoints.json` unchanged to the Windows owner. The sink exits after 900 seconds and writes a final snapshot beside the endpoint file. Do not expose it persistently.

### 3. Windows owner performs the first complete run

Place `offhost-endpoints.json` beside `run.ps1`, then:

```powershell
.\run.ps1
```

The runner refuses existing evidence, validates the matching token, starts the loopback companion sink, runs source and Windows compile gates, records every input hash, and executes the three-arm measurement once. Windows PowerShell 5.1 native stderr is handled with scoped EAP relaxation, immediate `$LASTEXITCODE` capture and restoration in `finally`.

Return unchanged:

- `sink-request.json`;
- `offhost-endpoints.json` and the off-host final snapshot;
- `loopback-endpoints.json` and its final snapshot if written;
- `measurement-config.json`;
- `webview2-realm-disable-results.json`;
- `webview2-realm-disable-run.log`;
- `sink-local.stdout.log` and `sink-local.stderr.log`;
- the extracted source archive hash.

The first complete result is final whether `PASS`, `FAIL` or `VOID`. Do not sample again for a greener answer.

## Verdict boundary

- `PASS`: both candidate-off realms and huddle controls are live at loopback and off-host sinks; the huddle token crosses; both protected realms lack a constructible peer connection; every protected counter is zero.
- `FAIL`: controls are live, but a protected constructor or sink path survives.
- `VOID`: any required report, huddle token, loopback control, off-host control, protocol-valid counter or nonce-bound TURN counter is absent.

The included certificate/key are public test fixtures, not credentials. All arms use the same recorded `--ignore-certificate-errors` argument solely to make the self-contained TURNS control measurable.
