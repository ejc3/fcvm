//! Agent connections. Each is newline-delimited JSON over a byte stream: a child process's
//! stdin and stdout (`fcvm fleet agent --stdio` locally, or the same through `ssh <host>`), or
//! the `/agent` WebSocket a GPU host's agent dials.
//!
//! A dialed agent names its host in its hello, and the coordinator checks that name
//! ([`AgentIdentity`]): with host templates configured, it must be an EC2 instance of this
//! fleet's hosts (`Purpose=fcvm-fleet-host`, `Fleet=<--fleet-name>`, pending or running), and
//! the connection must come from that instance (its public IPv4 address, or one of its IPv6
//! addresses or delegated prefixes, as `CF-Connecting-IP` names it). A host of another fleet
//! cannot join, and a machine holding a stolen agent token cannot pass for a host by naming
//! its instance id, which snapshots show. A refusal is logged at warn with
//! `marker=fleet-agent-refused`.

use super::hosts::HostDriver;
use super::{Cmd, Handle};
use crate::fleet::lines::{spawn_writer, LineReader};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use fleet_core::protocol::{AgentMsg, CoordMsg};
use std::net::IpAddr;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::oneshot;

static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

/// The log marker of an agent the coordinator refused (its Access token, or the host its hello
/// names), for an alarm.
pub const AGENT_REFUSED_MARKER: &str = "fleet-agent-refused";

/// Whether the host an agent's hello names may join this fleet.
#[async_trait]
pub trait AgentIdentity: Send + Sync {
    /// `Err` says why `host` may not join over a connection from `source` (`None`: unknown).
    async fn check(&self, host: &str, source: Option<IpAddr>) -> std::result::Result<(), String>;
}

/// An agent's host must be one of this fleet's GPU hosts: an EC2 instance, pending or running,
/// that the host driver lists in one of the host regions (`Purpose=fcvm-fleet-host`,
/// `Fleet=<fleet>`). A listing that fails refuses the agent; it dials again.
pub struct FleetHosts {
    pub driver: Arc<dyn HostDriver>,
    pub regions: Vec<String>,
    /// For the refusal's text.
    pub fleet: String,
}

#[async_trait]
impl AgentIdentity for FleetHosts {
    async fn check(&self, host: &str, source: Option<IpAddr>) -> std::result::Result<(), String> {
        let source = source.ok_or("the connection's source address is unknown")?;
        for region in &self.regions {
            let hosts = self
                .driver
                .hosts(region)
                .await
                .map_err(|e| format!("listing this fleet's hosts in {region}: {e:#}"))?;
            if let Some(h) = hosts.iter().find(|h| h.id == host) {
                if !h.live() {
                    return Err(format!("instance {host} is {}", h.state));
                }
                return if h.sources.iter().any(|s| owns(s, source)) {
                    Ok(())
                } else {
                    Err(format!(
                        "the connection comes from {source}, not from instance {host}"
                    ))
                };
            }
        }
        Err(format!(
            "{host} is not an instance tagged Purpose=fcvm-fleet-host and Fleet={} in {}",
            self.fleet,
            self.regions.join(", ")
        ))
    }
}

/// Whether `source` is `address` or inside the prefix `address/length`.
fn owns(address: &str, source: IpAddr) -> bool {
    let (addr, len) = match address.split_once('/') {
        Some((a, l)) => (a, l.parse::<u32>().ok()),
        None => (address, None),
    };
    let Ok(addr) = addr.parse::<IpAddr>() else {
        return false;
    };
    match (addr.to_canonical(), source.to_canonical(), len) {
        (a, s, None) => a == s,
        (IpAddr::V6(a), IpAddr::V6(s), Some(l)) if l <= 128 => {
            let mask = u128::MAX.checked_shl(128 - l).unwrap_or(0);
            u128::from(a) & mask == u128::from(s) & mask
        }
        (IpAddr::V4(a), IpAddr::V4(s), Some(l)) if l <= 32 => {
            let mask = u32::MAX.checked_shl(32 - l).unwrap_or(0);
            u32::from(a) & mask == u32::from(s) & mask
        }
        _ => false,
    }
}

/// Serve one agent connection until it closes. The first message must be a hello, and the
/// actor must accept the host it names.
pub async fn serve_connection<R, W>(input: R, output: W, handle: &Handle) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    serve_checked(input, output, handle, None, None).await
}

/// [`serve_connection`], with the hello's host checked by `identity` first against the
/// connection's `source` address.
pub async fn serve_checked<R, W>(
    input: R,
    output: W,
    handle: &Handle,
    identity: Option<&dyn AgentIdentity>,
    source: Option<IpAddr>,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let conn = NEXT_CONN.fetch_add(1, Ordering::Relaxed);
    let mut reader = LineReader::new(input, "agent");
    let hello = match reader.next::<AgentMsg>().await? {
        Some(m @ AgentMsg::Hello { .. }) => m,
        Some(other) => bail!("agent spoke before its hello: {other:?}"),
        None => bail!("agent closed before its hello"),
    };
    if let (Some(identity), AgentMsg::Hello { host, .. }) = (identity, &hello) {
        if let Err(why) = identity.check(host, source).await {
            tracing::warn!(
                marker = AGENT_REFUSED_MARKER,
                host = %host,
                reason = %why,
                "refusing an agent: its host is not one of this fleet's"
            );
            bail!("refused the agent for {host}: {why}");
        }
    }
    let (host_name, minecraft_servers) = match &hello {
        AgentMsg::Hello {
            host, minecraft, ..
        } => (host.clone(), *minecraft),
        _ => unreachable!("the first message is a hello"),
    };
    let (tx, writer) = spawn_writer::<CoordMsg, _>(output, "agent");
    let (reply, accepted) = oneshot::channel();
    handle
        .cmds
        .send(Cmd::AgentHello {
            conn,
            msg: hello,
            tx,
            reply,
        })
        .await
        .ok()
        .context("the coordinator stopped")?;
    if !accepted.await.unwrap_or(false) {
        bail!("the coordinator refused the agent's hello (it names another host than this connection)");
    }
    // This connection is the host's from now on: a connection it replaced, still open, no
    // longer publishes the host's Minecraft status rows or takes them away when it closes.
    handle
        .minecraft_status
        .claim(&host_name, conn, minecraft_servers);
    let result = loop {
        match reader.next::<AgentMsg>().await {
            // A host's Minecraft status rows go straight to the pages that asked: they are no
            // part of the state, so the actor never sees them.
            Ok(Some(AgentMsg::Health(fleet_core::protocol::HealthReport {
                minecraft: Some(status),
                ..
            }))) => handle.minecraft_status.publish(&host_name, conn, status),
            Ok(Some(msg)) => {
                if handle.cmds.send(Cmd::AgentMsg { conn, msg }).await.is_err() {
                    break Ok(());
                }
            }
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        }
    };
    let _ = handle.cmds.send(Cmd::AgentClosed { conn }).await;
    handle.minecraft_status.forget(&host_name, conn);
    writer.abort();
    result
}

/// Keep one agent command running: start it, serve its connection, and when it ends,
/// start it again after a backoff of 1 s doubling to 30 s (reset after a minute up).
pub async fn run_command(argv: Vec<String>, handle: Handle) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        match run_once(&argv, &handle).await {
            Ok(()) => tracing::warn!(cmd = ?argv, "agent connection ended"),
            Err(e) => {
                tracing::warn!(cmd = ?argv, error = %format!("{e:#}"), "agent connection failed")
            }
        }
        if started.elapsed() >= Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn run_once(argv: &[String], handle: &Handle) -> Result<()> {
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    arm_pdeathsig(cmd.as_std_mut());
    let mut child = cmd.spawn().with_context(|| format!("starting {argv:?}"))?;
    let stdin = child.stdin.take().context("agent stdin")?;
    let stdout = child.stdout.take().context("agent stdout")?;
    let result = serve_connection(stdout, stdin, handle).await;
    // The writer task held the agent's stdin and is gone, so the agent reads EOF and stops
    // its VMs; give it time to, then make sure it is gone.
    match tokio::time::timeout(Duration::from_secs(90), child.wait()).await {
        Ok(status) => tracing::info!(cmd = ?argv, status = ?status, "agent exited"),
        Err(_) => {
            tracing::warn!(cmd = ?argv, "agent did not exit 90 s after its input closed; killing it");
            let _ = child.kill().await;
        }
    }
    result
}

/// The coordinator's agents die with it: a local agent then tears its VMs down, and an
/// ssh client's exit closes the remote agent's input, which does the same.
fn arm_pdeathsig(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    let coord = std::process::id() as libc::pid_t;
    // SAFETY: two async-signal-safe calls in the forked child; no allocation.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != coord {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
}
