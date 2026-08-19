//! What kind of launch this is — best effort, for the deafness investigation.
//!
//! Both reports of "the pill says listening and the wake word is deaf" were the
//! first start after an in-app update, so which launch this is belongs in the
//! diagnostics beside the audio counters.
//!
//! ## What the updater actually leaves behind
//!
//! Nothing in the process itself, on the versions this app pins:
//!
//! * Windows (`tauri-plugin-updater` 2.10.1, the platform both repros came
//!   from) hands the NSIS installer `/UPDATE /ARGS <this process's args>` and
//!   then calls `std::process::exit(0)`; the installer relaunches the app with
//!   `RunAsUser "$INSTDIR\app.exe" "$ARGS"` — the *original* arguments, with no
//!   marker of its own and no environment variable.
//! * macOS and Linux go through `tauri::process::restart`, which spawns the
//!   binary with `env.args_os` minus `argv[0]`. Again no marker.
//!
//! So there is no in-process fact that says "the updater started me". What is
//! reliable is the version the previous launch left behind: a launch whose
//! version differs from the recorded one **is** the first launch after an
//! update, which is the class both repros belong to. It cannot distinguish an
//! updater relaunch from the user opening the freshly updated app themselves —
//! stated here rather than guessed at in the UI.
//!
//! The launch arguments are recorded verbatim (bounded) because they are the
//! one thing the NSIS path does pass through, and because a repro report that
//! carries them costs nothing to produce.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::managed_agents::storage::atomic_write_json_restricted;

/// Where the previous launch's version is remembered. Deliberately its own
/// file: `ambient-voice-settings.json` is the user's configuration, versioned
/// and migrated, and a diagnostic breadcrumb has no business in it.
pub(crate) const LAUNCH_FILE: &str = "ambient-launch.json";

/// Upper bounds on what is carried into the report. Arguments are attacker-free
/// (they come from the OS) but unbounded text on a status event that is emitted
/// on every session transition is still not worth it.
const MAX_LAUNCH_ARGS: usize = 8;
const MAX_LAUNCH_ARG_CHARS: usize = 120;

/// How this launch started, as far as the process can tell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchDiagnostics {
    /// The running build.
    pub version: String,
    /// What the previous launch recorded, or `None` on a first-ever launch (or
    /// when the breadcrumb could not be read).
    pub previous_version: Option<String>,
    /// The previous launch ran a different build. The updater's relaunch is one
    /// way to get here; the user opening the new build themselves is the other.
    pub first_launch_after_update: bool,
    /// `argv` after the program name, bounded. Empty for an ordinary launch;
    /// the Windows updater passes the pre-update process's arguments through.
    pub args: Vec<String>,
}

/// The breadcrumb on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LaunchRecord {
    version: String,
}

/// Compare this launch with the previous one. Pure, so the interesting cases
/// are testable without a filesystem or an app handle.
pub(crate) fn diagnose(
    version: &str,
    previous_version: Option<String>,
    args: Vec<String>,
) -> LaunchDiagnostics {
    let first_launch_after_update = previous_version
        .as_deref()
        .is_some_and(|previous| previous != version);
    LaunchDiagnostics {
        version: version.to_string(),
        previous_version,
        first_launch_after_update,
        args,
    }
}

/// `argv[1..]`, bounded in both directions.
fn launch_args() -> Vec<String> {
    std::env::args()
        .skip(1)
        .take(MAX_LAUNCH_ARGS)
        .map(|arg| arg.chars().take(MAX_LAUNCH_ARG_CHARS).collect())
        .collect()
}

pub(crate) fn launch_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|dir| dir.join(LAUNCH_FILE))
        .map_err(|error| format!("could not locate Buzz settings storage: {error}"))
}

/// Read what the last launch recorded and leave this launch's version in its
/// place. A breadcrumb that cannot be read or written is not an error worth
/// surfacing — the diagnostics simply say nothing about the previous launch.
pub(crate) fn exchange_recorded_version(path: &Path, version: &str) -> Option<String> {
    let previous = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<LaunchRecord>(&bytes).ok())
        .map(|record| record.version);
    if previous.as_deref() != Some(version) {
        let record = LaunchRecord {
            version: version.to_string(),
        };
        if let Ok(payload) = serde_json::to_vec_pretty(&record) {
            if let Err(error) = atomic_write_json_restricted(path, &payload) {
                eprintln!("buzz-desktop: ambient launch breadcrumb not written: {error}");
            }
        }
    }
    previous
}

/// Record this launch and describe it. Called once, from boot hydration.
pub fn detect(app: &AppHandle) -> LaunchDiagnostics {
    let version = app.package_info().version.to_string();
    let previous_version = launch_path(app)
        .ok()
        .and_then(|path| exchange_recorded_version(&path, &version));
    diagnose(&version, previous_version, launch_args())
}

#[cfg(test)]
#[path = "launch_tests.rs"]
mod launch_tests;
