//! Agent connections. Each is newline-delimited JSON over a byte stream; today the stream
//! is a child process's stdin and stdout (`fcvm fleet agent --stdio` locally, or the same
//! through `ssh <host>`).

use super::{Cmd, Handle};
use crate::fleet::lines::{spawn_writer, LineReader};
use anyhow::{bail, Context, Result};
use fleet_core::protocol::{AgentMsg, CoordMsg};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::oneshot;

static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

/// Serve one agent connection until it closes. The first message must be a hello, and the
/// actor must accept the host it names.
pub async fn serve_connection<R, W>(input: R, output: W, handle: &Handle) -> Result<()>
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
        bail!("the coordinator refused the agent's hello (its host is already connected)");
    }
    let result = loop {
        match reader.next::<AgentMsg>().await {
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
