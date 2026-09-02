#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    net::TcpListener,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use tauri::{WebviewUrl, WebviewWindowBuilder};
use tiny_http::{Header, Method, Response, Server};
use webview2_realm_disable_harness::{
    evaluate, fetch_snapshot, read_config, sha256_hex, write_result_once, LaneEndpoints,
    MeasurementConfig, WEBRTC_DISABLE_SCRIPT,
};

const BROWSER_ARGS: &str =
    "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --ignore-certificate-errors";
const MEASUREMENT_SECONDS: u64 = 38;

fn header(name: &str, value: &str) -> Option<Header> {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).ok()
}

fn response(body: Vec<u8>, content_type: &str, status: u16) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut response = Response::from_data(body).with_status_code(status);
    for (name, value) in [
        ("Content-Type", content_type),
        ("Cache-Control", "no-store"),
        ("Access-Control-Allow-Origin", "*"),
        (
            "Content-Security-Policy",
            "default-src 'none'; script-src 'self'; frame-src 'self'; connect-src 'self'; style-src 'unsafe-inline'",
        ),
    ] {
        if let Some(value) = header(name, value) {
            response = response.with_header(value);
        }
    }
    response
}

fn page_html(lane: &str) -> String {
    format!(
        "<!doctype html><meta charset=\"utf-8\"><title>{lane}</title><h1>{lane}</h1><pre id=\"status\">running</pre><script src=\"/probe.js?lane={lane}\"></script>"
    )
}

fn endpoint_value(host: &str, endpoints: &LaneEndpoints, token: &str, lane: &str) -> Value {
    let username = format!("buzz:{token}:{lane}");
    json!({
        "host": host,
        "stun_udp": format!("stun:{host}:{}", endpoints.stun_udp),
        "turn_udp": format!("turn:{host}:{}?transport=udp", endpoints.turn_udp),
        "turn_tcp": format!("turn:{host}:{}?transport=tcp", endpoints.turn_tcp),
        "turns_tls": format!("turns:{host}:{}?transport=tcp", endpoints.turns_tls),
        "username": username,
        "credential": "buzz-measurement-password"
    })
}

fn lane_config(config: &MeasurementConfig, lane: &str) -> Option<Value> {
    let local = config.loopback.lanes.get(lane)?;
    let off_host = config.off_host.lanes.get(lane)?;
    Some(json!({
        "token": config.token,
        "lane": lane,
        "loopback": endpoint_value(
            &config.loopback.advertised_host,
            local,
            &config.token,
            lane
        ),
        "offhost": endpoint_value(
            &config.off_host.advertised_host,
            off_host,
            &config.token,
            lane
        )
    }))
}

fn probe_javascript() -> &'static str {
    r##""use strict";
const scriptUrl = new URL(document.currentScript.src);
const lane = scriptUrl.searchParams.get("lane");
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const constructorNames = [
  "RTCPeerConnection", "webkitRTCPeerConnection", "mozRTCPeerConnection",
  "RTCDataChannel", "webkitRTCDataChannel", "mozRTCDataChannel"
];
const constructorTypes = Object.fromEntries(constructorNames.map(name => [name, typeof globalThis[name]]));
const Pc = globalThis.RTCPeerConnection || globalThis.webkitRTCPeerConnection || globalThis.mozRTCPeerConnection;

async function ice(name, url, username, credential) {
  const out = {name, url, constructed:false, offerCreated:false, setLocalResolved:false};
  if (typeof Pc !== "function") { out.error = "constructor-unavailable"; return out; }
  let pc;
  try {
    const server = {urls:[url]};
    if (username) { server.username = username; server.credential = credential; }
    pc = new Pc({iceServers:[server]});
    out.constructed = true;
    pc.createDataChannel("buzz-measurement");
    const offer = await pc.createOffer();
    out.offerCreated = true;
    await pc.setLocalDescription(offer);
    out.setLocalResolved = true;
    await sleep(5200);
    out.iceGatheringState = pc.iceGatheringState;
    out.localDescriptionPresent = !!pc.localDescription;
  } catch (error) {
    out.error = String(error);
  }
  if (pc) pc.close();
  return out;
}

async function localPair(token) {
  const out = {constructed:false, offerCreated:false, answerCreated:false, connected:false, nonceReceived:false};
  if (typeof Pc !== "function") return out;
  let left, right;
  try {
    left = new Pc(); right = new Pc(); out.constructed = true;
    left.onicecandidate = event => { if (event.candidate) right.addIceCandidate(event.candidate).catch(()=>{}); };
    right.onicecandidate = event => { if (event.candidate) left.addIceCandidate(event.candidate).catch(()=>{}); };
    right.ondatachannel = event => {
      event.channel.onmessage = message => { if (message.data === token) out.nonceReceived = true; };
    };
    const channel = left.createDataChannel("huddle-control");
    channel.onopen = () => { out.connected = true; channel.send(token); };
    const offer = await left.createOffer(); out.offerCreated = true;
    await left.setLocalDescription(offer); await right.setRemoteDescription(offer);
    const answer = await right.createAnswer(); out.answerCreated = true;
    await right.setLocalDescription(answer); await left.setRemoteDescription(answer);
    await sleep(4500);
  } catch (error) { out.error = String(error); }
  if (left) left.close(); if (right) right.close();
  return out;
}

(async () => {
  const config = await fetch(`/config/${encodeURIComponent(lane)}`, {cache:"no-store"}).then(response => response.json());
  const result = {scriptRan:true, lane, href:location.href, userAgent:navigator.userAgent, constructorTypes, probes:{}};
  if (lane.endsWith("-initial")) {
    const childLane = lane.replace(/-initial$/, "-srcdoc");
    const frame = document.createElement("iframe");
    frame.id = "srcdoc-bypass";
    frame.srcdoc = `<script src="${location.origin}/probe.js?lane=${childLane}"><\/script>`;
    document.body.append(frame);
  }
  for (const scope of ["loopback", "offhost"]) {
    const target = config[scope];
    result.probes[scope] = {};
    [
      result.probes[scope].stun_udp,
      result.probes[scope].turn_udp,
      result.probes[scope].turn_tcp,
      result.probes[scope].turns_tls
    ] = await Promise.all([
      ice("stun_udp", target.stun_udp),
      ice("turn_udp", target.turn_udp, `${target.username}:${scope}`, target.credential),
      ice("turn_tcp", target.turn_tcp, `${target.username}:${scope}`, target.credential),
      ice("turns_tls", target.turns_tls, `${target.username}:${scope}`, target.credential)
    ]);
  }
  if (lane === "huddle") result.localPair = await localPair(config.token);
  await fetch(`/report/${encodeURIComponent(lane)}`, {
    method:"POST", headers:{"content-type":"application/json"}, body:JSON.stringify(result)
  });
  const status = document.querySelector("#status");
  if (status) status.textContent = JSON.stringify(result, null, 2);
})().catch(async error => {
  const result = {scriptRan:true, lane, constructorTypes, fatalError:String(error), probes:{}};
  try { await fetch(`/report/${encodeURIComponent(lane)}`, {method:"POST",headers:{"content-type":"application/json"},body:JSON.stringify(result)}); } catch (_) {}
});
"##
}

fn start_page_server(
    listener: TcpListener,
    config: MeasurementConfig,
    reports: Arc<Mutex<BTreeMap<String, Value>>>,
) -> Result<u16, String> {
    let port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    let server = Server::from_listener(listener, None).map_err(|error| error.to_string())?;
    thread::spawn(move || {
        for mut request in server.incoming_requests() {
            let url = request.url().to_string();
            if request.method() == &Method::Post && url.starts_with("/report/") {
                let lane = url
                    .trim_start_matches("/report/")
                    .split('?')
                    .next()
                    .unwrap_or("unknown");
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);
                let value = serde_json::from_str(&body)
                    .unwrap_or_else(|_| json!({"invalidJson":body,"lane":lane}));
                if let Ok(mut guard) = reports.lock() {
                    guard.insert(lane.to_string(), value);
                }
                let _ = request.respond(response(b"ok".to_vec(), "text/plain", 200));
                continue;
            }
            if let Some(lane) = url.strip_prefix("/config/") {
                let lane = lane.split('?').next().unwrap_or(lane);
                match lane_config(&config, lane) {
                    Some(value) => {
                        let body = serde_json::to_vec(&value).unwrap_or_default();
                        let _ = request.respond(response(body, "application/json", 200));
                    }
                    None => {
                        let _ =
                            request.respond(response(b"missing lane".to_vec(), "text/plain", 404));
                    }
                }
                continue;
            }
            let (body, content_type, status) = if url.starts_with("/probe.js") {
                (
                    probe_javascript().as_bytes().to_vec(),
                    "text/javascript",
                    200,
                )
            } else if url.starts_with("/arm/protected") {
                (
                    page_html("protected-initial").into_bytes(),
                    "text/html; charset=utf-8",
                    200,
                )
            } else if url.starts_with("/arm/control") {
                (
                    page_html("control-initial").into_bytes(),
                    "text/html; charset=utf-8",
                    200,
                )
            } else if url.starts_with("/huddle") {
                (
                    page_html("huddle").into_bytes(),
                    "text/html; charset=utf-8",
                    200,
                )
            } else {
                (b"not found".to_vec(), "text/plain", 404)
            };
            let _ = request.respond(response(body, content_type, status));
        }
    });
    Ok(port)
}

fn main() {
    if !cfg!(windows) {
        eprintln!(
            "This harness is runtime-valid only on Windows/WebView2. Linux source checks do not constitute acceptance."
        );
        return;
    }
    let cwd = match std::env::current_dir() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("could not resolve working directory: {error}");
            return;
        }
    };
    let config_path = std::env::var_os("WEBVIEW2_SUCCESSOR_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| cwd.join("measurement-config.json"));
    let config_bytes = match fs::read(&config_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("could not read measurement config: {error}");
            return;
        }
    };
    let config = match read_config(&config_path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("measurement config refused: {error}");
            return;
        }
    };
    let result_path = cwd.join("webview2-realm-disable-results.json");
    if result_path.exists() {
        eprintln!("existing result found; preserve it and use a fresh extraction");
        return;
    }
    let listener = match TcpListener::bind(("127.0.0.1", 0)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("could not bind page server: {error}");
            return;
        }
    };
    let reports = Arc::new(Mutex::new(BTreeMap::new()));
    let page_port = match start_page_server(listener, config.clone(), reports.clone()) {
        Ok(port) => port,
        Err(error) => {
            eprintln!("could not start page server: {error}");
            return;
        }
    };
    let data_root = cwd
        .join("target")
        .join("webview2-realm-disable")
        .join(&config.token);
    if let Err(error) = fs::create_dir_all(&data_root) {
        eprintln!("could not create UDF root: {error}");
        return;
    }

    let setup_config = config.clone();
    let final_config = config.clone();
    let final_reports = reports.clone();
    let final_result_path = result_path.clone();
    let final_data_root = data_root.clone();
    let config_hash = sha256_hex(&config_bytes);
    let injection_hash = sha256_hex(WEBRTC_DISABLE_SCRIPT.as_bytes());

    let builder = tauri::Builder::default().setup(move |app| {
        let origin = format!("http://127.0.0.1:{page_port}");
        let protected_url = format!("{origin}/arm/protected");
        let control_url = format!("{origin}/arm/control");
        let huddle_url = format!("{origin}/huddle");

        WebviewWindowBuilder::new(
            app,
            "protected-extension",
            WebviewUrl::External(protected_url.parse()?),
        )
        .title("Protected extension candidate")
        .data_directory(data_root.join("protected-extension"))
        .additional_browser_args(BROWSER_ARGS)
        .initialization_script_for_all_frames(WEBRTC_DISABLE_SCRIPT)
        .inner_size(760.0, 560.0)
        .build()?;

        WebviewWindowBuilder::new(
            app,
            "control-extension",
            WebviewUrl::External(control_url.parse()?),
        )
        .title("Candidate-off extension control")
        .data_directory(data_root.join("control-extension"))
        .additional_browser_args(BROWSER_ARGS)
        .inner_size(760.0, 560.0)
        .build()?;

        WebviewWindowBuilder::new(
            app,
            "huddle-control",
            WebviewUrl::External(huddle_url.parse()?),
        )
        .title("Normal Buzz huddle control")
        .data_directory(data_root.join("huddle"))
        .additional_browser_args(BROWSER_ARGS)
        .inner_size(760.0, 560.0)
        .build()?;

        let handle = app.handle().clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(MEASUREMENT_SECONDS));
            let report_snapshot = final_reports
                .lock()
                .map(|guard| guard.clone())
                .unwrap_or_default();
            let loopback = fetch_snapshot(&final_config.loopback).ok();
            let off_host = fetch_snapshot(&final_config.off_host).ok();
            let mut result = evaluate(
                &final_config.token,
                &report_snapshot,
                loopback.as_ref(),
                off_host.as_ref(),
            );
            if let Some(object) = result.as_object_mut() {
                object.insert("platform".into(), json!(std::env::consts::OS));
                object.insert("architecture".into(), json!(std::env::consts::ARCH));
                object.insert("token".into(), json!(final_config.token));
                object.insert("measurement_config_sha256".into(), json!(config_hash));
                object.insert("injected_script_sha256".into(), json!(injection_hash));
                object.insert("browser_arguments".into(), json!(BROWSER_ARGS));
                object.insert("tauri_version".into(), json!("2.11.5"));
                object.insert("wry_version".into(), json!("0.55.1"));
                object.insert("webview2_com_version".into(), json!("0.38.2"));
                object.insert(
                    "user_data_folders".into(),
                    json!({
                        "protected": final_data_root.join("protected-extension"),
                        "control": final_data_root.join("control-extension"),
                        "huddle": final_data_root.join("huddle")
                    }),
                );
                object.insert(
                    "candidate".into(),
                    json!({
                        "mechanism":"tauri initialization_script_for_all_frames / WebView2 document-created injection",
                        "protected_only":true,
                        "production_migration":false
                    }),
                );
            }
            let rendered = serde_json::to_string_pretty(&result)
                .unwrap_or_else(|error| format!("{{\"overall\":\"ERROR\",\"error\":{error:?}}}"));
            if let Err(error) = write_result_once(&final_result_path, &result) {
                eprintln!("could not write first result: {error}");
            }
            println!(
                "\n==== WEBVIEW2 REALM-DISABLE MEASUREMENT ====\n{rendered}\n==== END MEASUREMENT ===="
            );
            let exit = if result["overall"] == "PASS" { 0 } else { 2 };
            handle.exit(exit);
        });
        let _ = setup_config;
        Ok(())
    });

    if let Err(error) = builder.run(tauri::generate_context!()) {
        eprintln!("WebView2 successor harness failed: {error}");
    }
}
