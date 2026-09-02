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
      files.bridge.includes(
        "native_window::caller_authorized(label, lease, url.as_str())",
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
    files.wrapper.match(/new MessageChannel\(\)/g)?.length === 1 &&
      !files.wrapper.includes("event.ports"),
    "wrapper no longer solely originates one MessageChannel",
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
      native: actual.native.replace(
        " && record.wrapper_url == wrapper_url",
        "",
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
