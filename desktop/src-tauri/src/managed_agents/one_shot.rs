//! Tool-free one-shot execution through Buzz's configured managed-agent
//! provider boundary.
//!
//! Only the bundled `buzz-agent one-shot-no-tools` sidecar is eligible. That
//! subcommand reuses the owner's configured provider/model but supplies no MCP
//! or built-in tool definitions and performs exactly one provider request.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tauri::AppHandle;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::watch;

use super::discovery::discover_acp_runtimes_from;
use super::global_config::load_global_agent_config;
use super::runtime::terminate_process;
use super::types::AcpAvailabilityStatus;

const MAX_SIDECAR_REPLY_BYTES: u64 = 16 * 1024;
const TURN_TIMEOUT: Duration = Duration::from_secs(32);

#[derive(Debug)]
struct ConfiguredRuntime {
    binary: PathBuf,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SidecarReply {
    message: String,
}

fn resolve_configured_runtime<R: tauri::Runtime>(
    app: &AppHandle<R>,
) -> Result<ConfiguredRuntime, String> {
    let global = load_global_agent_config(app).map_err(|_| "agent_not_configured".to_string())?;
    let runtime_id = global
        .preferred_runtime
        .as_deref()
        .filter(|id| *id == "buzz-agent")
        .ok_or_else(|| "agent_not_configured".to_string())?;
    let custom_dir = super::managed_agents_base_dir(app)
        .map_err(|_| "agent_not_configured".to_string())?
        .join("custom-harnesses");
    let runtimes = discover_acp_runtimes_from(Some(&custom_dir), true);
    let runtime = runtimes
        .into_iter()
        .find(|candidate| {
            candidate.id == runtime_id
                && candidate.availability == AcpAvailabilityStatus::Available
        })
        .ok_or_else(|| "agent_not_configured".to_string())?;
    let binary = runtime
        .binary_path
        .map(PathBuf::from)
        .ok_or_else(|| "agent_not_configured".to_string())?;

    let mut env = runtime.definition_env;
    env.extend(global.env_vars);
    if let (Some(key), Some(value)) = (runtime.provider_env_var.as_deref(), global.provider) {
        env.insert(key.to_string(), value);
    }
    if let (Some(key), Some(value)) = (runtime.model_env_var.as_deref(), global.model) {
        env.insert(key.to_string(), value);
    }
    // The isolated sidecar receives only the effective harness config plus the
    // minimum OS path/home context needed by TLS and provider OAuth caches.
    for key in [
        "HOME",
        "USER",
        "PATH",
        "SystemRoot",
        "LOCALAPPDATA",
        "APPDATA",
        "XDG_CONFIG_HOME",
    ] {
        if !env.contains_key(key) {
            if let Ok(value) = std::env::var(key) {
                env.insert(key.to_string(), value);
            }
        }
    }

    let cwd = super::default_agent_workdir()
        .ok_or_else(|| "agent_not_configured".to_string())?;
    Ok(ConfiguredRuntime {
        binary,
        args: runtime.default_args,
        env,
        cwd,
    })
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.as_std_mut().process_group(0);
}

#[cfg(windows)]
fn configure_process_group(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

async fn terminate_runtime(mut child: Child) {
    if let Some(pid) = child.id() {
        let _ = tokio::task::spawn_blocking(move || terminate_process(pid)).await;
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

async fn run_sidecar(
    runtime: ConfiguredRuntime,
    prompt: String,
    mut cancel: watch::Receiver<bool>,
) -> Result<String, String> {
    let mut command = Command::new(&runtime.binary);
    command
        .args(&runtime.args)
        .arg("one-shot-no-tools")
        .current_dir(&runtime.cwd)
        .env_clear()
        .envs(&runtime.env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    configure_process_group(&mut command);
    let mut child = command.spawn().map_err(|_| "agent_unavailable".to_string())?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "agent_unavailable".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "agent_unavailable".to_string())?;
    let request = serde_json::to_vec(&json!({
        "system": "You are a bounded mathematical learning assistant. Use only the supplied object context. Probe before teaching, keep the reply concise, do not claim learning evidence, and do not request or use tools.",
        "prompt": prompt,
    }))
    .map_err(|_| "agent_unavailable".to_string())?;
    stdin
        .write_all(&request)
        .await
        .map_err(|_| "agent_unavailable".to_string())?;
    stdin.shutdown().await.ok();
    drop(stdin);

    let read_output = async {
        let mut output = Vec::new();
        stdout
            .take(MAX_SIDECAR_REPLY_BYTES + 1)
            .read_to_end(&mut output)
            .await
            .map_err(|_| "agent_unavailable".to_string())?;
        Ok::<Vec<u8>, String>(output)
    };
    tokio::pin!(read_output);
    let output = tokio::select! {
        _ = cancel.changed() => {
            terminate_runtime(child).await;
            return Err("agent_cancelled".to_string());
        }
        result = tokio::time::timeout(TURN_TIMEOUT, &mut read_output) => {
            match result {
                Ok(Ok(output)) => output,
                Ok(Err(error)) => { terminate_runtime(child).await; return Err(error); }
                Err(_) => { terminate_runtime(child).await; return Err("agent_timeout".to_string()); }
            }
        }
    };
    let status = match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(_)) => return Err("agent_unavailable".to_string()),
        Err(_) => {
            terminate_runtime(child).await;
            return Err("agent_timeout".to_string());
        }
    };
    if !status.success() || output.len() as u64 > MAX_SIDECAR_REPLY_BYTES {
        return Err("agent_unavailable".to_string());
    }
    let reply: SidecarReply = serde_json::from_slice(&output)
        .map_err(|_| "agent_unavailable".to_string())?;
    let message = reply.message.trim();
    if message.is_empty() || message.len() > MAX_SIDECAR_REPLY_BYTES as usize {
        return Err("agent_unavailable".to_string());
    }
    Ok(message.to_string())
}

pub(crate) async fn converse<R: tauri::Runtime>(
    app: AppHandle<R>,
    prompt: String,
    cancel: watch::Receiver<bool>,
) -> Result<String, String> {
    let runtime = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::task::spawn_blocking(move || resolve_configured_runtime(&app)),
    )
    .await
    .map_err(|_| "agent_timeout".to_string())?
    .map_err(|_| "agent_unavailable".to_string())??;
    run_sidecar(runtime, prompt, cancel).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_runtime(script: &str) -> ConfiguredRuntime {
        ConfiguredRuntime {
            binary: PathBuf::from("python3"),
            args: vec!["-u".into(), "-c".into(), script.into(), "--".into()],
            env: std::env::vars().collect(),
            cwd: std::env::temp_dir(),
        }
    }

    #[tokio::test]
    async fn configured_boundary_returns_one_bounded_reply() {
        let script = "import json,sys; request=json.load(sys.stdin); assert 'prompt' in request; print(json.dumps({'message':'Scoped reply'}),end='')";
        let (_sender, receiver) = watch::channel(false);
        let reply = run_sidecar(fixture_runtime(script), "bounded prompt".into(), receiver)
            .await
            .expect("reply");
        assert_eq!(reply, "Scoped reply");
    }

    #[tokio::test]
    async fn teardown_cancels_an_inflight_turn() {
        let script = "import json,sys,time; json.load(sys.stdin); time.sleep(60)";
        let (sender, receiver) = watch::channel(false);
        let task = tokio::spawn(run_sidecar(
            fixture_runtime(script),
            "bounded prompt".into(),
            receiver,
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        sender.send(true).expect("cancel");
        assert_eq!(task.await.expect("join").unwrap_err(), "agent_cancelled");
    }

    #[tokio::test]
    async fn oversized_sidecar_output_is_rejected() {
        let script = "import json,sys; json.load(sys.stdin); print('x'*17000,end='')";
        let (_sender, receiver) = watch::channel(false);
        assert_eq!(
            run_sidecar(fixture_runtime(script), "bounded prompt".into(), receiver)
                .await
                .unwrap_err(),
            "agent_unavailable"
        );
    }
}
