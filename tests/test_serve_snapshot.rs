//! `fcvm serve` starts each sandbox as an `fcvm podman run` child, so a sandbox restores
//! from the snapshot cache the way the CLI does, `--no-snapshot` boots it cold, and the
//! children do not outlive the server.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{bail, Context, Result};
use fcvm::state::{HealthStatus, ProcessType, StateManager, VmState};
use std::path::Path;
use std::time::{Duration, Instant};

/// Start `fcvm serve --port 0` and return the child and the base URL it listens on.
///
/// `snapshots_enabled` drops an inherited `FCVM_NO_SNAPSHOT` (the CI SnapshotDisabled lane
/// sets it) from the server and so from every sandbox it starts.
async fn start_serve(
    extra: &[&str],
    snapshots_enabled: bool,
) -> Result<(tokio::process::Child, String)> {
    let mut args = vec!["serve", "--port", "0"];
    args.extend_from_slice(extra);
    let (child, _pid, log_path) = if snapshots_enabled {
        common::spawn_fcvm_snapshots_enabled_with_env_and_log_path(&args, &[]).await?
    } else {
        common::spawn_fcvm_with_env_and_log_path(&args, &[]).await?
    };
    let port = wait_for_listen_port(&log_path, Duration::from_secs(30)).await?;
    Ok((child, format!("http://127.0.0.1:{port}")))
}

/// The port from serve's `Listening addr=127.0.0.1:<port>` line.
async fn wait_for_listen_port(log_path: &Path, timeout: Duration) -> Result<u16> {
    let deadline = Instant::now() + timeout;
    loop {
        let log = std::fs::read_to_string(log_path).unwrap_or_default();
        for line in log.lines().filter(|l| l.contains("Listening")) {
            if let Some(rest) = line.split("addr=127.0.0.1:").nth(1) {
                let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(port) = digits.parse() {
                    return Ok(port);
                }
            }
        }
        if Instant::now() > deadline {
            bail!("serve did not log its listening address within {timeout:?}: {log_path:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn create(client: &reqwest::Client, base: &str) -> Result<String> {
    let resp: serde_json::Value = client
        .post(format!("{base}/v1/sandboxes"))
        .json(&serde_json::json!({ "image": common::TEST_IMAGE }))
        .timeout(Duration::from_secs(300))
        .send()
        .await?
        .json()
        .await?;
    anyhow::ensure!(resp["success"] == true, "create failed: {resp}");
    Ok(resp["data"]["sandboxId"]
        .as_str()
        .context("create returned no sandboxId")?
        .to_string())
}

async fn destroy(client: &reqwest::Client, base: &str, id: &str) -> Result<()> {
    let resp: serde_json::Value = client
        .delete(format!("{base}/v1/sandboxes/{id}"))
        .timeout(Duration::from_secs(120))
        .send()
        .await?
        .json()
        .await?;
    anyhow::ensure!(resp["success"] == true, "destroy failed: {resp}");
    Ok(())
}

async fn run_command(client: &reqwest::Client, base: &str, id: &str, cmd: &str) -> Result<String> {
    let resp: serde_json::Value = client
        .post(format!("{base}/s/{id}/run/command"))
        .json(&serde_json::json!({ "command": cmd }))
        .timeout(Duration::from_secs(60))
        .send()
        .await?
        .json()
        .await?;
    anyhow::ensure!(
        resp["data"]["exit_code"] == 0,
        "command {cmd:?} failed: {resp}"
    );
    Ok(resp["data"]["stdout"]
        .as_str()
        .unwrap_or_default()
        .to_string())
}

async fn state_of(id: &str) -> Result<VmState> {
    StateManager::new(fcvm::paths::state_dir())
        .load_state(id)
        .await
}

/// Whether `pid` names a process that has not exited (a zombie has).
fn alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => !matches!(
            stat.rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next()),
            Some("Z") | Some("X") | None
        ),
        Err(_) => false,
    }
}

async fn wait_gone(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    !alive(pid)
}

/// The second sandbox of an image is restored from the snapshot the first one left in the
/// cache, it runs commands, and a destroy ends its `fcvm podman run` child.
#[tokio::test(flavor = "multi_thread")]
async fn serve_restores_sandboxes_from_the_snapshot_cache() -> Result<()> {
    let (mut serve, base) = start_serve(&[], true).await?;
    let client = reqwest::Client::new();

    let first = create(&client, &base).await?;
    destroy(&client, &base, &first).await?;

    let second = create(&client, &base).await?;
    let state = state_of(&second).await?;
    assert_eq!(state.health_status, HealthStatus::Healthy);
    assert_eq!(
        state.config.process_type,
        Some(ProcessType::Clone),
        "the second sandbox of an image must be restored from the snapshot cache, \
         got process_type {:?} snapshot {:?}",
        state.config.process_type,
        state.config.snapshot_name
    );
    assert!(state.config.snapshot_name.is_some());

    let out = run_command(&client, &base, &second, "echo serve-ok").await?;
    assert_eq!(out.trim(), "serve-ok");

    let child_pid = state.pid.context("sandbox state has no pid")?;
    assert!(alive(child_pid));
    destroy(&client, &base, &second).await?;
    assert!(
        !alive(child_pid),
        "destroy returned while the sandbox's fcvm process {child_pid} was still running"
    );

    common::kill_process(serve.id().context("serve pid")?).await;
    let _ = serve.wait().await;
    Ok(())
}

/// With `--no-snapshot` a sandbox boots cold and takes nothing from the cache.
#[tokio::test(flavor = "multi_thread")]
async fn serve_no_snapshot_boots_sandboxes_cold() -> Result<()> {
    let (mut serve, base) = start_serve(&["--no-snapshot"], true).await?;
    let client = reqwest::Client::new();

    let id = create(&client, &base).await?;
    let state = state_of(&id).await?;
    assert_eq!(state.health_status, HealthStatus::Healthy);
    assert_eq!(state.config.process_type, Some(ProcessType::Vm));
    assert_eq!(state.config.snapshot_name, None);
    let out = run_command(&client, &base, &id, "echo cold-ok").await?;
    assert_eq!(out.trim(), "cold-ok");
    destroy(&client, &base, &id).await?;

    common::kill_process(serve.id().context("serve pid")?).await;
    let _ = serve.wait().await;
    Ok(())
}

/// A SIGKILLed server cannot stop its sandboxes, so the kernel has to: every child is armed
/// with a parent-death signal, and it tears its VM down on it.
#[tokio::test(flavor = "multi_thread")]
async fn sigkill_of_serve_ends_its_sandboxes() -> Result<()> {
    let (mut serve, base) = start_serve(&[], false).await?;
    let client = reqwest::Client::new();

    let id = create(&client, &base).await?;
    let state = state_of(&id).await?;
    let child_pid = state.pid.context("sandbox state has no pid")?;
    assert!(alive(child_pid));

    serve.start_kill()?;
    let _ = serve.wait().await;

    assert!(
        wait_gone(child_pid, Duration::from_secs(60)).await,
        "sandbox fcvm process {child_pid} outlived the SIGKILLed server"
    );
    Ok(())
}
