//! One fcvm child process running one slot's GPU VM.
//!
//! The agent starts `fcvm podman run ...` and owns the child: it reads both pipes to the
//! end (an unread pipe stalls fcvm once 64 KiB are buffered), takes the restore timings
//! from fcvm's one-line `restored` record on stdout, and is the only one to signal or reap
//! it. The child is armed with `PR_SET_PDEATHSIG`, so if the agent dies the kernel sends
//! fcvm SIGTERM and fcvm tears its VM down.

use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::watch;

/// How one slot's VM is started.
#[derive(Debug, Clone)]
pub struct VmSpec {
    pub fcvm: PathBuf,
    pub name: String,
    pub sbdf: String,
    /// `<ip>:<port>:8080`
    pub publish: String,
    pub extra_args: Vec<String>,
    pub image: String,
}

impl VmSpec {
    pub fn args(&self) -> Vec<String> {
        let mut a: Vec<String> = [
            "podman",
            "run",
            "--name",
            &self.name,
            "--kernel-profile",
            "gpu",
            "--gpu",
            &self.sbdf,
            "--publish",
            &self.publish,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        a.extend(self.extra_args.iter().cloned());
        a.push(self.image.clone());
        a
    }
}

/// What fcvm's `restored` record says about the restore.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Restored {
    pub at: Instant,
    pub ack_ms: Option<u64>,
    pub gpu_ms: Option<u64>,
}

/// Parse a stdout line; `Some` for fcvm's `{"status":"restored",...}` record.
pub fn parse_restored(line: &str, at: Instant) -> Option<Restored> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if v.get("status")?.as_str()? != "restored" {
        return None;
    }
    let ms = |v: Option<&serde_json::Value>| v.and_then(|x| x.as_f64()).map(|x| x.round() as u64);
    Some(Restored {
        at,
        ack_ms: ms(v.get("ack_ms")),
        gpu_ms: ms(v.pointer("/guest_phases/gpu_ms")),
    })
}

const TAIL_LINES: usize = 20;

/// Resolves when fcvm prints its restore record; pending forever if it never does.
pub async fn wait_restored(rx: &mut watch::Receiver<Option<Restored>>) -> Restored {
    loop {
        if let Some(r) = *rx.borrow_and_update() {
            return r;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

pub struct Vm {
    child: Child,
    pub pid: u32,
    pub started: Instant,
    restored: watch::Receiver<Option<Restored>>,
    tail: Arc<Mutex<VecDeque<String>>>,
    exit: Option<ExitStatus>,
}

impl Vm {
    pub fn spawn(spec: &VmSpec, slot: &str) -> Result<Vm> {
        let mut cmd = Command::new(&spec.fcvm);
        cmd.args(spec.args())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        arm_pdeathsig(cmd.as_std_mut());
        let mut child = cmd
            .spawn()
            .with_context(|| format!("starting {}", spec.fcvm.display()))?;
        let pid = child.id().context("fcvm exited before its pid was read")?;
        let (tx, restored) = watch::channel(None);
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let stdout = child.stdout.take().context("fcvm stdout")?;
        let stderr = child.stderr.take().context("fcvm stderr")?;
        let slot_out = slot.to_string();
        let tail_out = tail.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(r) = parse_restored(&line, Instant::now()) {
                    let _ = tx.send(Some(r));
                }
                tracing::debug!(target: "fcvm::fleet::vm", slot = %slot_out, "stdout: {line}");
                push_tail(&tail_out, line);
            }
        });
        let slot_err = slot.to_string();
        let tail_err = tail.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "fcvm::fleet::vm", slot = %slot_err, "{line}");
                push_tail(&tail_err, line);
            }
        });
        Ok(Vm {
            child,
            pid,
            started: Instant::now(),
            restored,
            tail,
            exit: None,
        })
    }

    /// The restore record, once fcvm has printed it.
    pub fn restored(&self) -> Option<Restored> {
        *self.restored.borrow()
    }

    /// A receiver of the restore record, for [`wait_restored`].
    pub fn restored_watch(&self) -> watch::Receiver<Option<Restored>> {
        self.restored.clone()
    }

    /// Resolves when the fcvm process exits. Cancel safe.
    pub async fn exited(&mut self) -> ExitStatus {
        if let Some(s) = self.exit {
            return s;
        }
        let s = match self.child.wait().await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(pid = self.pid, error = %e, "waiting for fcvm failed");
                std::future::pending().await
            }
        };
        self.exit = Some(s);
        s
    }

    /// The last lines fcvm printed, for a failure report.
    pub fn tail(&self) -> String {
        let t = self.tail.lock().unwrap();
        t.iter().cloned().collect::<Vec<_>>().join("\n")
    }

    /// SIGTERM, wait up to `grace`, then SIGKILL; returns once the process has exited and
    /// been reaped.
    pub async fn stop(&mut self, grace: Duration) -> ExitStatus {
        if let Some(s) = self.exit {
            return s;
        }
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(self.pid as i32),
            nix::sys::signal::Signal::SIGTERM,
        );
        match tokio::time::timeout(grace, self.exited()).await {
            Ok(s) => s,
            Err(_) => {
                tracing::warn!(
                    pid = self.pid,
                    "fcvm ignored SIGTERM for {grace:?}; killing it"
                );
                let _ = self.child.start_kill();
                self.exited().await
            }
        }
    }
}

/// A Vm dropped while fcvm runs (the agent returning, a slot task ending) sends it
/// SIGTERM, which tears the VM down, and does not wait. Not SIGKILL (tokio's
/// `kill_on_drop`): a SIGKILLed fcvm leaves a routed VM's namespace, veth and host route
/// behind, and the next VM given that address is refused.
impl Drop for Vm {
    fn drop(&mut self) {
        // try_wait reaps an exited child, so the pid signalled below is still fcvm's.
        if self.exit.is_none() && matches!(self.child.try_wait(), Ok(None)) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(self.pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
    }
}

fn push_tail(tail: &Mutex<VecDeque<String>>, line: String) {
    let mut t = tail.lock().unwrap();
    t.push_back(line);
    while t.len() > TAIL_LINES {
        t.pop_front();
    }
}

/// The kernel sends the child SIGTERM when the agent thread that spawned it dies. The
/// spawning threads are the runtime's workers, which live as long as the agent.
fn arm_pdeathsig(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    let agent = std::process::id() as libc::pid_t;
    // SAFETY: runs in the forked child before exec and makes two async-signal-safe calls;
    // both error paths build an errno-only io::Error, which does not allocate.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // The agent died between the fork and the prctl: start nothing unsupervised.
            if libc::getppid() != agent {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restored_record_gives_ack_and_gpu_times() {
        let now = Instant::now();
        let r = parse_restored(
            r#"{"status":"restored","name":"fleet-slot-1","ack_ms":4157,"guest_phases":{"gpu_ms":3337.4,"total_ms":4100}}"#,
            now,
        )
        .unwrap();
        assert_eq!((r.ack_ms, r.gpu_ms), (Some(4157), Some(3337)));
        let bare = parse_restored(
            r#"{"status":"restored","ack_ms":10,"guest_phases":null}"#,
            now,
        )
        .unwrap();
        assert_eq!((bare.ack_ms, bare.gpu_ms), (Some(10), None));
        assert!(parse_restored("✓ VM 'x' cloned from snapshot 'y'", now).is_none());
        assert!(parse_restored(r#"{"status":"ready"}"#, now).is_none());
    }

    /// A Vm dropped without `stop` (the agent returning, a slot task ending) must give
    /// fcvm SIGTERM, which tears the VM down, not SIGKILL: a SIGKILLed routed fcvm leaves
    /// its namespace, veth and host route, and the next VM at that address is refused.
    #[tokio::test]
    async fn dropping_a_vm_gives_fcvm_sigterm() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mark = dir.path().join("term");
        let script = dir.path().join("fake-fcvm");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ntrap 'echo term > {}; exit 0' TERM\necho ready\nwhile :; do sleep 0.05; done\n",
                mark.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let spec = VmSpec {
            fcvm: script,
            name: "fleet-slot-1".into(),
            sbdf: "0000:02:00.0".into(),
            publish: "127.0.0.1:20001:8080".into(),
            extra_args: vec![],
            image: "renderer:1".into(),
        };
        let vm = Vm::spawn(&spec, "slot-1").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !vm.tail().contains("ready") {
            assert!(Instant::now() < deadline, "the fake fcvm never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        drop(vm);
        while !mark.exists() {
            assert!(
                Instant::now() < deadline,
                "fcvm got no SIGTERM when its Vm was dropped"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[test]
    fn spec_builds_the_documented_command() {
        let s = VmSpec {
            fcvm: "fcvm".into(),
            name: "fleet-slot-2".into(),
            sbdf: "0000:02:00.0".into(),
            publish: "127.0.0.1:20001:8080".into(),
            extra_args: vec!["--cpu".into(), "4".into()],
            image: "renderer:1".into(),
        };
        assert_eq!(
            s.args().join(" "),
            "podman run --name fleet-slot-2 --kernel-profile gpu --gpu 0000:02:00.0 --publish 127.0.0.1:20001:8080 --cpu 4 renderer:1"
        );
    }
}
