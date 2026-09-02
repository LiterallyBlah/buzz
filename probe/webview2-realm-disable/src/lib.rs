use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    path::Path,
    time::Duration,
};

pub const ROW_NAME: &str = "extension_environment_webrtc_disable_is_realm_complete_and_scoped";
pub const LANES: [&str; 5] = [
    "protected-initial",
    "protected-srcdoc",
    "control-initial",
    "control-srcdoc",
    "huddle",
];
pub const TRANSPORTS: [&str; 4] = ["stun_udp", "turn_udp", "turn_tcp", "turns_tls"];
pub const CONSTRUCTORS: [&str; 6] = [
    "RTCPeerConnection",
    "webkitRTCPeerConnection",
    "mozRTCPeerConnection",
    "RTCDataChannel",
    "webkitRTCDataChannel",
    "mozRTCDataChannel",
];

/// Document-created script installed by the host into every frame of the
/// protected WebView2 environment. This is a measurement candidate, not
/// production policy. The exact bytes are hashed into every result.
pub const WEBRTC_DISABLE_SCRIPT: &str = r#"(() => {
  "use strict";
  const names = [
    "RTCPeerConnection", "webkitRTCPeerConnection", "mozRTCPeerConnection",
    "RTCDataChannel", "webkitRTCDataChannel", "mozRTCDataChannel"
  ];
  for (const name of names) {
    try {
      Object.defineProperty(globalThis, name, {
        value: undefined,
        writable: false,
        enumerable: false,
        configurable: false
      });
    } catch (_) {
      try { globalThis[name] = undefined; } catch (_) {}
    }
  }
})();"#;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct LaneEndpoints {
    pub stun_udp: u16,
    pub turn_udp: u16,
    pub turn_tcp: u16,
    pub turns_tls: u16,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SinkEndpoints {
    pub schema: String,
    pub token: String,
    pub advertised_host: String,
    pub control_port: u16,
    pub lanes: BTreeMap<String, LaneEndpoints>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MeasurementConfig {
    pub schema: String,
    pub token: String,
    pub loopback: SinkEndpoints,
    pub off_host: SinkEndpoints,
}

impl MeasurementConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != "buzz-webview2-realm-disable-config/v1" {
            return Err("unexpected measurement config schema".into());
        }
        if self.token.len() < 24
            || self.token != self.loopback.token
            || self.token != self.off_host.token
        {
            return Err("sink tokens must match the bounded run token".into());
        }
        if self.loopback.advertised_host != "127.0.0.1" {
            return Err("loopback sink must advertise literal 127.0.0.1".into());
        }
        if matches!(
            self.off_host.advertised_host.as_str(),
            "127.0.0.1" | "localhost"
        ) {
            return Err("off-host sink must not advertise loopback".into());
        }
        for lane in LANES {
            if !self.loopback.lanes.contains_key(lane) || !self.off_host.lanes.contains_key(lane) {
                return Err(format!("missing endpoint lane: {lane}"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProtocolCounter {
    #[serde(default)]
    pub packets: u64,
    #[serde(default)]
    pub valid: u64,
    #[serde(default)]
    pub nonce_bound: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct LaneCounters {
    #[serde(default)]
    pub stun_udp: ProtocolCounter,
    #[serde(default)]
    pub turn_udp: ProtocolCounter,
    #[serde(default)]
    pub turn_tcp: ProtocolCounter,
    #[serde(default)]
    pub turns_tls: ProtocolCounter,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SinkSnapshot {
    pub schema: String,
    pub token: String,
    pub lanes: BTreeMap<String, LaneCounters>,
}

impl SinkSnapshot {
    pub fn validate(&self, token: &str) -> bool {
        self.schema == "buzz-controlled-webrtc-sink-snapshot/v1"
            && self.token == token
            && LANES.iter().all(|lane| self.lanes.contains_key(*lane))
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn read_config(path: &Path) -> Result<MeasurementConfig, String> {
    let bytes = fs::read(path).map_err(|error| format!("could not read config: {error}"))?;
    let config: MeasurementConfig =
        serde_json::from_slice(&bytes).map_err(|error| format!("invalid config JSON: {error}"))?;
    config.validate()?;
    Ok(config)
}

pub fn write_result_once(path: &Path, result: &Value) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|error| format!("result path already exists or cannot be created: {error}"))?;
    let mut rendered = serde_json::to_vec_pretty(result).map_err(|error| error.to_string())?;
    rendered.push(b'\n');
    file.write_all(&rendered)
        .map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())
}

fn value_bool(value: Option<&Value>, pointer: &str) -> bool {
    value
        .and_then(|item| item.pointer(pointer))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn value_str<'a>(value: Option<&'a Value>, pointer: &str) -> Option<&'a str> {
    value
        .and_then(|item| item.pointer(pointer))
        .and_then(Value::as_str)
}

fn report_has_all_constructor_types(report: Option<&Value>) -> bool {
    CONSTRUCTORS.iter().all(|name| {
        value_str(report, &format!("/constructorTypes/{name}"))
            .is_some_and(|kind| matches!(kind, "function" | "undefined" | "object"))
    })
}

fn report_control_live(report: Option<&Value>) -> bool {
    value_bool(report, "/scriptRan")
        && report_has_all_constructor_types(report)
        && TRANSPORTS.iter().all(|transport| {
            ["loopback", "offhost"].iter().all(|scope| {
                let prefix = format!("/probes/{scope}/{transport}");
                value_bool(report, &format!("{prefix}/constructed"))
                    && value_bool(report, &format!("{prefix}/offerCreated"))
                    && value_bool(report, &format!("{prefix}/setLocalResolved"))
            })
        })
}

fn report_protected_blocked(report: Option<&Value>) -> bool {
    value_bool(report, "/scriptRan")
        && report_has_all_constructor_types(report)
        && [
            "RTCPeerConnection",
            "webkitRTCPeerConnection",
            "mozRTCPeerConnection",
        ]
        .iter()
        .all(|name| value_str(report, &format!("/constructorTypes/{name}")) != Some("function"))
        && TRANSPORTS.iter().all(|transport| {
            ["loopback", "offhost"].iter().all(|scope| {
                !value_bool(report, &format!("/probes/{scope}/{transport}/constructed"))
            })
        })
}

fn lane_positive(snapshot: &SinkSnapshot, lane: &str) -> bool {
    snapshot.lanes.get(lane).is_some_and(|counts| {
        counts.stun_udp.valid > 0
            && counts.turn_udp.valid > 0
            && counts.turn_udp.nonce_bound > 0
            && counts.turn_tcp.valid > 0
            && counts.turn_tcp.nonce_bound > 0
            && counts.turns_tls.valid > 0
            && counts.turns_tls.nonce_bound > 0
    })
}

fn lane_zero(snapshot: &SinkSnapshot, lane: &str) -> bool {
    snapshot.lanes.get(lane).is_some_and(|counts| {
        [
            &counts.stun_udp,
            &counts.turn_udp,
            &counts.turn_tcp,
            &counts.turns_tls,
        ]
        .iter()
        .all(|counter| counter.packets == 0 && counter.valid == 0 && counter.nonce_bound == 0)
    })
}

/// Evaluate the single successor row. An unavailable/dead off-host control is
/// VOID before any protected negative can receive credit.
pub fn evaluate(
    token: &str,
    reports: &BTreeMap<String, Value>,
    loopback: Option<&SinkSnapshot>,
    off_host: Option<&SinkSnapshot>,
) -> Value {
    let matrix_complete = LANES.iter().all(|lane| reports.contains_key(*lane));
    let snapshots_valid =
        loopback.is_some_and(|s| s.validate(token)) && off_host.is_some_and(|s| s.validate(token));
    let control_reports_live = ["control-initial", "control-srcdoc"]
        .iter()
        .all(|lane| report_control_live(reports.get(*lane)));
    let huddle_report_live = report_control_live(reports.get("huddle"))
        && value_bool(reports.get("huddle"), "/localPair/nonceReceived");
    let loopback_controls_live = loopback.is_some_and(|snapshot| {
        ["control-initial", "control-srcdoc", "huddle"]
            .iter()
            .all(|lane| lane_positive(snapshot, lane))
    });
    let offhost_controls_live = off_host.is_some_and(|snapshot| {
        ["control-initial", "control-srcdoc", "huddle"]
            .iter()
            .all(|lane| lane_positive(snapshot, lane))
    });

    let validity_live = matrix_complete
        && snapshots_valid
        && control_reports_live
        && huddle_report_live
        && loopback_controls_live
        && offhost_controls_live;

    let protected_reports_blocked = ["protected-initial", "protected-srcdoc"]
        .iter()
        .all(|lane| report_protected_blocked(reports.get(*lane)));
    let protected_sinks_zero = loopback.is_some_and(|snapshot| {
        ["protected-initial", "protected-srcdoc"]
            .iter()
            .all(|lane| lane_zero(snapshot, lane))
    }) && off_host.is_some_and(|snapshot| {
        ["protected-initial", "protected-srcdoc"]
            .iter()
            .all(|lane| lane_zero(snapshot, lane))
    });

    let status = if !validity_live {
        "VOID"
    } else if protected_reports_blocked && protected_sinks_zero {
        "PASS"
    } else {
        "FAIL"
    };

    json!({
        "schema": "buzz-webview2-realm-disable-measurement/v1",
        "overall": status,
        "rows": [{
            "name": ROW_NAME,
            "status": status,
            "evidence": {
                "matrix_complete": matrix_complete,
                "snapshots_valid": snapshots_valid,
                "candidate_off_reports_live": control_reports_live,
                "huddle_report_live": huddle_report_live,
                "loopback_controls_live": loopback_controls_live,
                "offhost_controls_live": offhost_controls_live,
                "protected_reports_blocked": protected_reports_blocked,
                "protected_sinks_zero": protected_sinks_zero,
                "reports": reports,
                "loopback_snapshot": loopback,
                "offhost_snapshot": off_host
            }
        }],
        "notes": [
            "VOID means a candidate-off or huddle validity control was missing, dead, nonce-invalid, or could not reach the off-host sink.",
            "PASS requires constructor denial in both protected realms and zero protected loopback and off-host sink traffic.",
            "This successor does not import or reinterpret the completed predecessor harness result."
        ]
    })
}

pub fn fetch_snapshot(endpoints: &SinkEndpoints) -> Result<SinkSnapshot, String> {
    let address = (endpoints.advertised_host.as_str(), endpoints.control_port)
        .to_socket_addrs()
        .map_err(|error| format!("could not resolve sink control endpoint: {error}"))?
        .next()
        .ok_or_else(|| "sink control endpoint had no address".to_string())?;
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5))
        .map_err(|error| format!("could not connect to sink control endpoint: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| error.to_string())?;
    let request = format!(
        "GET /snapshot/{} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\n\r\n",
        endpoints.token, endpoints.advertised_host, endpoints.control_port
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|error| error.to_string())?;
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|error| error.to_string())?;
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| "sink control response had no HTTP body".to_string())?;
    let head = String::from_utf8_lossy(&response[..split]);
    if !head.starts_with("HTTP/1.0 200") && !head.starts_with("HTTP/1.1 200") {
        return Err(format!("sink control returned non-200 response: {head}"));
    }
    serde_json::from_slice(&response[split + 4..])
        .map_err(|error| format!("invalid sink snapshot JSON: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process;

    fn live_report(huddle: bool) -> Value {
        let mut constructor_types = serde_json::Map::new();
        for name in CONSTRUCTORS {
            constructor_types.insert(name.into(), Value::String("function".into()));
        }
        let probe = json!({"constructed":true,"offerCreated":true,"setLocalResolved":true});
        let mut scopes = serde_json::Map::new();
        for scope in ["loopback", "offhost"] {
            let mut transports = serde_json::Map::new();
            for transport in TRANSPORTS {
                transports.insert(transport.into(), probe.clone());
            }
            scopes.insert(scope.into(), Value::Object(transports));
        }
        json!({
            "scriptRan":true,
            "constructorTypes":constructor_types,
            "probes":scopes,
            "localPair":{"nonceReceived":huddle}
        })
    }

    fn blocked_report() -> Value {
        let mut constructor_types = serde_json::Map::new();
        for name in CONSTRUCTORS {
            constructor_types.insert(name.into(), Value::String("undefined".into()));
        }
        let probe = json!({"constructed":false,"offerCreated":false,"setLocalResolved":false});
        let mut scopes = serde_json::Map::new();
        for scope in ["loopback", "offhost"] {
            let mut transports = serde_json::Map::new();
            for transport in TRANSPORTS {
                transports.insert(transport.into(), probe.clone());
            }
            scopes.insert(scope.into(), Value::Object(transports));
        }
        json!({"scriptRan":true,"constructorTypes":constructor_types,"probes":scopes})
    }

    fn snapshot(token: &str, protected_zero: bool) -> SinkSnapshot {
        let mut lanes = BTreeMap::new();
        for lane in LANES {
            let positive = !protected_zero || !lane.starts_with("protected-");
            let counter = if positive {
                ProtocolCounter {
                    packets: 1,
                    valid: 1,
                    nonce_bound: 1,
                }
            } else {
                ProtocolCounter::default()
            };
            lanes.insert(
                lane.into(),
                LaneCounters {
                    stun_udp: ProtocolCounter {
                        nonce_bound: 0,
                        ..counter.clone()
                    },
                    turn_udp: counter.clone(),
                    turn_tcp: counter.clone(),
                    turns_tls: counter,
                },
            );
        }
        SinkSnapshot {
            schema: "buzz-controlled-webrtc-sink-snapshot/v1".into(),
            token: token.into(),
            lanes,
        }
    }

    #[test]
    fn pass_requires_complete_realms_live_controls_and_zero_protected_sinks() {
        let token = "0123456789abcdef0123456789abcdef";
        let reports = BTreeMap::from([
            ("protected-initial".into(), blocked_report()),
            ("protected-srcdoc".into(), blocked_report()),
            ("control-initial".into(), live_report(false)),
            ("control-srcdoc".into(), live_report(false)),
            ("huddle".into(), live_report(true)),
        ]);
        let local = snapshot(token, true);
        let remote = snapshot(token, true);
        assert_eq!(
            evaluate(token, &reports, Some(&local), Some(&remote))["overall"],
            "PASS"
        );
    }

    #[test]
    fn dead_offhost_control_is_void_never_pass() {
        let token = "0123456789abcdef0123456789abcdef";
        let reports = BTreeMap::from([
            ("protected-initial".into(), blocked_report()),
            ("protected-srcdoc".into(), blocked_report()),
            ("control-initial".into(), live_report(false)),
            ("control-srcdoc".into(), live_report(false)),
            ("huddle".into(), live_report(true)),
        ]);
        let local = snapshot(token, true);
        assert_eq!(
            evaluate(token, &reports, Some(&local), None)["overall"],
            "VOID"
        );
    }

    #[test]
    fn protected_constructor_or_sink_reach_is_fail() {
        let token = "0123456789abcdef0123456789abcdef";
        let reports = BTreeMap::from([
            ("protected-initial".into(), live_report(false)),
            ("protected-srcdoc".into(), blocked_report()),
            ("control-initial".into(), live_report(false)),
            ("control-srcdoc".into(), live_report(false)),
            ("huddle".into(), live_report(true)),
        ]);
        let local = snapshot(token, false);
        let remote = snapshot(token, true);
        assert_eq!(
            evaluate(token, &reports, Some(&local), Some(&remote))["overall"],
            "FAIL"
        );
    }

    #[test]
    fn write_once_refuses_result_overwrite() {
        let path = std::env::temp_dir().join(format!(
            "webview2-realm-disable-write-once-{}.json",
            process::id()
        ));
        let _ = fs::remove_file(&path);
        write_result_once(&path, &json!({"overall":"PASS"})).expect("first write");
        assert!(write_result_once(&path, &json!({"overall":"FAIL"})).is_err());
        fs::remove_file(path).expect("remove fixture");
    }

    #[test]
    fn injected_script_names_every_constructor_and_runner_is_ps51_safe() {
        for name in CONSTRUCTORS {
            assert!(WEBRTC_DISABLE_SCRIPT.contains(name));
        }
        let main_source = include_str!("main.rs");
        for lane in ["protected-initial", "control-initial", "huddle"] {
            assert!(main_source.contains(lane));
        }
        assert!(main_source.contains("replace(/-initial$/, \"-srcdoc\")"));
        assert!(main_source.contains("/probe.js?lane="));
        assert!(main_source.contains("initialization_script_for_all_frames"));
        let runner = include_str!("../run.ps1");
        assert!(runner.starts_with("$ErrorActionPreference = 'Stop'"));
        assert!(runner.contains("$SavedErrorActionPreference = $ErrorActionPreference"));
        assert!(runner.contains("$NativeExit = $LASTEXITCODE"));
        assert!(runner
            .contains("finally {\n        $ErrorActionPreference = $SavedErrorActionPreference"));
        assert!(!runner.starts_with("$ErrorActionPreference = 'Continue'"));
    }
}
