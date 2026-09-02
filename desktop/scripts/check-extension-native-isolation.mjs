import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";

const EXPECTED_SCRIPT =
  "c4328966c35974dc87a7a43a55b470633819d38956289e33601072e47c319324";
const root = new URL("../", import.meta.url);
const read = (path) => readFileSync(new URL(path, root), "utf8");

const actual = {
  native: read("src-tauri/src/extensions/native_window.rs"),
  authority: read("src-tauri/src/extensions/frame_authority.rs"),
  bridge: read("src-tauri/src/extensions/bridge.rs"),
  query: read("src-tauri/src/extensions/query/connection.rs"),
  extensionMod: read("src-tauri/src/extensions/mod.rs"),
  wrapper: read("src-tauri/src/extensions/native_wrapper.js"),
  huddle: read("src-tauri/src/huddle/window.rs"),
  capability: read("src-tauri/capabilities/extension-native-bridge.json"),
};

function requireCondition(condition, message) {
  if (!condition) throw new Error(message);
}

function validate(files) {
  const script = files.native.match(
    /pub\(crate\) const WEBRTC_DISABLE_SCRIPT: &str = r#"([\s\S]*?)"#;/,
  )?.[1];
  requireCondition(script, "accepted all-frame script constant is missing");
  requireCondition(
    createHash("sha256").update(script).digest("hex") === EXPECTED_SCRIPT,
    "accepted all-frame script bytes drifted",
  );
  for (const name of [
    "RTCPeerConnection",
    "webkitRTCPeerConnection",
    "mozRTCPeerConnection",
    "RTCDataChannel",
    "webkitRTCDataChannel",
    "mozRTCDataChannel",
  ]) {
    requireCondition(
      script.includes(`"${name}"`),
      `missing constructor ${name}`,
    );
  }
  const production = files.native.split("#[cfg(test)]")[0] ?? files.native;
  requireCondition(
    production.match(/\.initialization_script_for_all_frames\(/g)?.length === 1,
    "Windows builder lost or widened all-frame injection",
  );
  requireCondition(
    !files.huddle.includes("initialization_script_for_all_frames"),
    "main/huddle WebRTC environment received the extension script",
  );
  requireCondition(
    production.includes(".data_directory(plan.data_directory.clone())"),
    "Windows builder lost its exact UDF",
  );
  requireCondition(
    production.includes("identity_directory(&authority.identity_pubkey)"),
    "UDF lost identity binding",
  );
  for (const fragment of [
    ".join(&authority.extension_id)",
    ".join(&authority.package_digest)",
    ".join(authority.grant_generation.to_string())",
    ".join(&label)",
  ]) {
    requireCondition(production.includes(fragment), `UDF lost ${fragment}`);
  }
  requireCondition(
    !production.includes(["ignore", "certificate", "errors"].join("-")) &&
      !production.includes(["disable", "non", "proxied", "udp"].join("_")) &&
      !production.includes(["additional", "browser", "args"].join("_")),
    "measurement-only browser arguments entered production",
  );
  requireCondition(
    files.authority.includes("owner.caller_label == caller_label"),
    "lease authority lost invoking-label binding",
  );
  requireCondition(
    files.bridge.includes("lease_authority_for_caller(lease, label)") &&
      files.bridge.includes("native_ready_for_caller(") &&
      files.bridge.includes(
        "caller_authorized_parts(label, Some(wrapper_url), lease)",
      ) &&
      files.native.includes("record.wrapper_url == wrapper_url"),
    "plugin command lost label/lease/origin/path admission",
  );
  const capability = JSON.parse(files.capability);
  requireCondition(
    capability.local === false,
    "native capability became local",
  );
  requireCondition(
    JSON.stringify(capability.windows) ===
      JSON.stringify(["extension-secure-*"]),
    "native capability label scope widened",
  );
  requireCondition(
    JSON.stringify(capability.remote?.urls) ===
      JSON.stringify(["http://127.0.0.1:*/frame/*"]),
    "native capability URL scope widened",
  );
  requireCondition(
    JSON.stringify(capability.platforms) === JSON.stringify(["windows"]),
    "native capability escaped Windows",
  );
  requireCondition(
    capability.permissions.length === 4 &&
      capability.permissions.every((permission) =>
        permission.startsWith("extension-bridge:"),
      ) &&
      capability.permissions.includes(
        "extension-bridge:allow-native-stream-bind",
      ) &&
      !files.capability.includes("core:event"),
    "native capability regained generic event authority",
  );
  requireCondition(
    files.wrapper.match(/new MessageChannel\(\)/g)?.length === 1 &&
      !files.wrapper.includes("event.ports") &&
      !files.wrapper.includes("plugin:event|") &&
      !files.wrapper.includes("__TAURI_EVENT_PLUGIN_INTERNALS__") &&
      files.wrapper.includes("plugin:extension-bridge|native_stream_bind") &&
      files.wrapper.includes("__TAURI_TO_IPC_KEY__") &&
      files.wrapper.includes("__CHANNEL__:"),
    "wrapper no longer solely originates one MessageChannel",
  );
  requireCondition(
    files.query.includes("route_by_branch_with_sink") &&
      files.query.includes("close_for_connection_with_sinks") &&
      files.query.includes("native_window::stream_sink_for_lease(lease)"),
    "stream delivery lost its subscription-owned native sink",
  );
  requireCondition(
    files.native.includes(
      "async fn close_native_extension_window_serialized",
    ) &&
      files.native.includes(
        "let _open_guard = NATIVE_OPEN_LOCK.lock().await;",
      ) &&
      files.native.indexOf("NATIVE_OPEN_LOCK.lock().await") <
        files.native.indexOf("lifecycle_read_fence().await") &&
      files.native.includes("cleanup_record_if_state(") &&
      files.native.includes("NativeWindowState::Opening"),
    "native close/open/watchdog serialization drifted",
  );
  requireCondition(
    files.extensionMod.includes(
      "if mode != native_window::ExtensionSurfaceMode::LinuxIframe",
    ) &&
      files.extensionMod.indexOf(
        "if mode != native_window::ExtensionSurfaceMode::LinuxIframe",
      ) < files.extensionMod.indexOf("lifecycle_read_fence().await"),
    "Windows can reach the legacy iframe opener",
  );
}

validate(actual);

const mutants = [
  {
    name: "remove-all-frame-injection",
    files: {
      ...actual,
      native: actual.native.replace(
        ".initialization_script_for_all_frames(plan.initialization_script)",
        ".initialization_script(plan.initialization_script)",
      ),
    },
  },
  {
    name: "remove-exact-udf",
    files: {
      ...actual,
      native: actual.native.replace(
        ".data_directory(plan.data_directory.clone())",
        "/* data directory removed */",
      ),
    },
  },
  {
    name: "remove-caller-label-binding",
    files: {
      ...actual,
      authority: actual.authority.replace(
        ".filter(|owner| owner.caller_label == caller_label)",
        "",
      ),
    },
  },
  {
    name: "remove-wrapper-origin-path-binding",
    files: {
      ...actual,
      native: actual.native.replaceAll(
        "record.wrapper_url == wrapper_url",
        "true",
      ),
    },
  },
  {
    name: "widen-capability-to-extension-child",
    files: {
      ...actual,
      capability: actual.capability.replace(
        "http://127.0.0.1:*/frame/*",
        "http://127.0.0.1:*/*",
      ),
    },
  },
  {
    name: "restore-generic-event-permission",
    files: {
      ...actual,
      capability: actual.capability.replace(
        '"extension-bridge:allow-native-stream-bind"',
        '"extension-bridge:allow-native-stream-bind", "core:event:allow-listen"',
      ),
    },
  },
  {
    name: "restore-global-stream-routing",
    files: {
      ...actual,
      query: actual.query.replace(
        "registry().route_by_branch_with_sink",
        "registry().route_by_branch",
      ),
    },
  },
  {
    name: "remove-close-serialization",
    files: {
      ...actual,
      native: actual.native.replace(
        "async fn close_native_extension_window_serialized",
        "async fn close_native_extension_window_unserialized",
      ),
    },
  },
  {
    name: "remove-ready-url-admission",
    files: {
      ...actual,
      bridge: actual.bridge.replace(
        "caller_authorized_parts(label, Some(wrapper_url), lease)",
        "frame_host::lease_authority_for_caller(lease, label).is_some()",
      ),
    },
  },
  {
    name: "restore-windows-legacy-iframe",
    files: {
      ...actual,
      extensionMod: actual.extensionMod.replace(
        "if mode != native_window::ExtensionSurfaceMode::LinuxIframe",
        "if false",
      ),
    },
  },
];

for (const mutant of mutants) {
  let refused = false;
  try {
    validate(mutant.files);
  } catch {
    refused = true;
  }
  requireCondition(refused, `mutation survived: ${mutant.name}`);
}

console.log(
  JSON.stringify({
    result: "PASS",
    acceptedScriptSha256: EXPECTED_SCRIPT,
    mutationsRefused: mutants.map((mutant) => mutant.name),
  }),
);
