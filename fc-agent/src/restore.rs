use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Context;
use tokio::sync::{watch, Notify};

use crate::network;
use crate::output::OutputHandle;

async fn wait_for_exec_rebind(
    done: &AtomicBool,
    done_notify: &Notify,
    timeout: std::time::Duration,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if done.load(Ordering::Acquire) {
            return Ok(());
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            anyhow::bail!(
                "exec server did not re-register within {:?}; refusing restore readiness",
                timeout
            );
        }
        // The AtomicBool is authoritative. Notify only avoids polling latency,
        // and the bounded wait lets us re-check the flag even if a notification
        // was consumed by an older waiter.
        let wait = (deadline - now).min(std::time::Duration::from_millis(50));
        let _ = tokio::time::timeout(wait, done_notify.notified()).await;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestoreState {
    Pending,
    Succeeded,
    Failed,
}

/// Shared restore outcome and the output-readiness edge it guards.
#[derive(Clone)]
pub struct RestoreStatus {
    state: watch::Sender<RestoreState>,
    output: OutputHandle,
}

impl RestoreStatus {
    pub fn new(output: OutputHandle) -> Self {
        let (state, _receiver) = watch::channel(RestoreState::Pending);
        Self { state, output }
    }

    /// Start a new restore epoch. Failed is absorbing because the clone is
    /// already shutting down and must never recover output readiness.
    pub fn begin(&self) -> anyhow::Result<()> {
        let mut failed = false;
        self.state.send_if_modified(|state| match *state {
            RestoreState::Pending => false,
            RestoreState::Succeeded => {
                *state = RestoreState::Pending;
                true
            }
            RestoreState::Failed => {
                failed = true;
                false
            }
        });
        if failed {
            anyhow::bail!("cannot begin a restore after restore state Failed");
        }
        Ok(())
    }

    pub fn fail(&self) {
        self.state.send_replace(RestoreState::Failed);
    }

    /// Complete the current restore and publish output readiness as one ordered
    /// transition. The watch value is changed before `reconnect`, while waiters
    /// are notified only after this closure returns, so every observer that sees
    /// Succeeded also knows the reconnect request has already been issued.
    pub fn succeed(&self) -> anyhow::Result<()> {
        let mut previous = RestoreState::Pending;
        let transitioned = self.state.send_if_modified(|state| {
            previous = *state;
            if *state != RestoreState::Pending {
                return false;
            }
            *state = RestoreState::Succeeded;
            self.output.reconnect();
            true
        });
        if !transitioned {
            anyhow::bail!(
                "cannot complete pending restore: current state is {:?}",
                previous
            );
        }
        Ok(())
    }

    /// Wait until the restore handler has either published output readiness or
    /// failed closed. This is the only WarmStart readiness gate.
    pub async fn wait_for_output_readiness(&self) -> anyhow::Result<()> {
        let mut state = self.state.subscribe();
        loop {
            match *state.borrow_and_update() {
                RestoreState::Pending => {}
                RestoreState::Succeeded => return Ok(()),
                RestoreState::Failed => {
                    anyhow::bail!("restore failed before output readiness")
                }
            }
            state
                .changed()
                .await
                .context("restore state publisher stopped before output readiness")?;
        }
    }
}

/// All signals needed for snapshot restore coordination.
///
/// Groups the exec rebind, egress reconnect, and output reconnect signals
/// that are passed between agent.rs, mmds.rs, and restore.rs.
pub struct RestoreSignals {
    pub restore_status: RestoreStatus,
    pub restore_flag: Arc<AtomicBool>,
    pub exec_rebind: Arc<Notify>,
    pub exec_rebind_needed: Arc<AtomicBool>,
    pub exec_rebind_done: Arc<AtomicBool>,
    pub exec_rebind_done_notify: Arc<Notify>,
    pub egress_gen_rx: Option<watch::Receiver<u64>>,
    /// Incremented by the output writer when it observes EPOLLERR on its
    /// established vsock connection — the guest-visible edge of the device's
    /// VIRTIO_VSOCK_EVENT_TRANSPORT_RESET at snapshot restore. The epoch
    /// watcher uses it as a wakeup to fast-poll for the new restore-epoch
    /// instead of finishing a frozen 50ms sleep. Accelerator only: the normal
    /// poll cadence remains the correctness path.
    pub vsock_reset_rx: watch::Receiver<u64>,
    /// NFS mounts from the boot-time plan, kept for post-restore remounting.
    /// MMDS can't be re-fetched here: the host's restore-epoch PUT replaces the
    /// whole MMDS store, so `container-plan` is gone by the time restore runs.
    /// This cache lives in the snapshot's memory image, so a restored VM sees
    /// exactly the mounts that were active when the snapshot was taken.
    pub nfs_mounts: Vec<crate::types::NfsMount>,
}

/// Handle clone restore: kill stale sockets, refresh gateway ARP, re-register exec,
/// and prepare output readiness.
///
/// CRITICAL ordering: exec re-register and egress reconnect MUST complete before
/// this function returns. Its caller transitions [`RestoreStatus`] to Succeeded,
/// which requests the output reconnect that the host treats as readiness. If
/// exec's AsyncFd epoll is still stale, health checks hang for ~60s. If egress
/// proxy hasn't reconnected, tests that immediately use egress after health
/// check will fail.
///
/// FUSE volumes are NOT remounted here. The reconnectable multiplexer
/// detects the dead vsock and auto-reconnects to the clone's VolumeServer.
/// The kernel FUSE session stays alive — processes see a brief hang, not errors.
///
/// `clone_ipv6`: For routed mode, the unique per-clone IPv6 that replaces the
/// snapshot's shared guest IPv6 on eth0. Without this, all clones share the same
/// IPv6 and return traffic gets ECMP-routed to the wrong clone.
pub async fn handle_clone_restore(
    signals: &RestoreSignals,
    clone_ipv6: Option<&str>,
    egress_gen_before: Option<u64>,
    restore_epoch: &str,
    transport: crate::bootplan::Transport,
) -> anyhow::Result<()> {
    let restore_started = std::time::Instant::now();
    eprintln!("[fc-agent] handling restore (epoch={})", restore_epoch);

    // Sync clock FIRST — snapshot restore leaves the VM clock frozen at snapshot time.
    // Services that validate timestamps (auth, TLS, sessions) will fail with stale time.
    // Use the active transport: Cloud Hypervisor has no MMDS, so an MMDS fetch would just
    // wait out its timeout and leave the clock stuck at snapshot time.
    if let Err(e) = crate::bootplan::sync_clock_from_host(transport).await {
        eprintln!("[fc-agent] WARNING: clock sync on restore failed: {:?}", e);
    }

    // Reset chrony after clock jump so it doesn't lose its sources.
    // The MMDS sync above stepped the clock, which confuses chrony's
    // offset tracking. `makestep` forces it to accept the new time.
    let _ = tokio::process::Command::new("chronyc")
        .args(["makestep"])
        .output()
        .await;

    // Reconfigure IPv6 — before any network traffic can use the old address.
    if let Some(new_ipv6) = clone_ipv6 {
        network::reconfigure_ipv6(new_ipv6).await;
    }

    let tcp_cleanup_started = std::time::Instant::now();
    eprintln!(
        "[fc-agent] restore phase=tcp-cleanup epoch={} begin",
        restore_epoch
    );
    crate::snapshot_network::restore_snapshot_network()
        .await
        .context("restore phase tcp-cleanup")?;
    eprintln!(
        "[fc-agent] restore phase=tcp-cleanup epoch={} complete elapsed_ms={:.3}",
        restore_epoch,
        tcp_cleanup_started.elapsed().as_secs_f64() * 1000.0
    );

    // Do not flush the neighbor table. A client can already be using an entry
    // while restore cleanup runs, and `ip neigh flush all` has no generation
    // boundary. One active ARP exchange refreshes the gateway and teaches the
    // new bridge/pasta path without deleting unrelated/current neighbors.
    let neighbor_started = std::time::Instant::now();
    eprintln!(
        "[fc-agent] restore phase=neighbor-refresh epoch={} begin",
        restore_epoch
    );
    network::refresh_gateway_arp().await;
    eprintln!(
        "[fc-agent] restore phase=neighbor-refresh epoch={} complete elapsed_ms={:.3}",
        restore_epoch,
        neighbor_started.elapsed().as_secs_f64() * 1000.0
    );

    // Remount NFS shares: their kernel TCP connections to the host's NFS
    // server died with the snapshot transport reset, and a hard NFS mount
    // wedges every accessor until remounted. Lazy-unmount then mount fresh
    // re-establishes the connection against the host's re-created export.
    // Uses the boot-time plan cached in RestoreSignals — see its doc comment
    // for why MMDS can't be consulted here.
    if !signals.nfs_mounts.is_empty() {
        eprintln!(
            "[fc-agent] remounting {} NFS share(s) after restore",
            signals.nfs_mounts.len()
        );
        for share in &signals.nfs_mounts {
            let _ = tokio::process::Command::new("umount")
                .args(["-l", &share.mount_path])
                .output()
                .await;
        }
        if let Err(e) = crate::mounts::mount_nfs_shares(&signals.nfs_mounts) {
            eprintln!(
                "[fc-agent] WARNING: NFS remount after restore failed: {:?}",
                e
            );
        }
    }

    // FIRST: Re-register exec server listener (AsyncFd epoll stale after transport reset).
    // Reset confirmation flag, then signal. Set flag BEFORE notify to prevent race
    // where select! drops the Notified future (see exec.rs doc comment).
    signals.exec_rebind_done.store(false, Ordering::Release);
    signals.exec_rebind_needed.store(true, Ordering::Release);
    signals.exec_rebind.notify_one();

    // SECOND: Wait for exec server to confirm re-register completed.
    // This ensures accept() works before the host can reach the exec server.
    // Wait on the AtomicBool (the source of truth) and use the Notify only as a
    // wakeup: a stale stored permit (e.g. left over from a previous restore whose
    // wait timed out before the rebind finished) must not let this restore proceed
    // before its own re-register has completed. The flag was reset above, so it only
    // reads true once the exec server has re-registered for THIS restore.
    wait_for_exec_rebind(
        &signals.exec_rebind_done,
        &signals.exec_rebind_done_notify,
        std::time::Duration::from_secs(5),
    )
    .await
    .context("waiting for exec server re-registration after restore")?;
    eprintln!("[fc-agent] exec re-registered after restore");

    // THIRD: Wait for egress proxy to reconnect (watch channel incremented after vsock connect).
    // No explicit signal needed — the proxy detects the dead vsock fd natively via
    // Interest::ERROR (EPOLLERR fires instantly after transport reset). The proxy's
    // select! arm fires, the session exits, and the reconnect loop connects a new vsock.
    // If proxy already reconnected, wait_for returns immediately (watch retains latest value).
    if let (Some(rx), Some(gen_before)) = (&signals.egress_gen_rx, egress_gen_before) {
        crate::proxy::wait_for_egress_gen(
            rx,
            gen_before,
            std::time::Duration::from_secs(5),
            "reconnected after restore",
        )
        .await
        .context("waiting for egress proxy reconnection after restore")?;
    }

    // FOURTH: Restart journald. The journal file was mid-write when the snapshot
    // was taken, so the restored journald finds a corrupted file and gets stuck.
    // systemd's watchdog would kill it after 3 min, but we restart it immediately
    // so other services can log via journald right away.
    restart_journald().await;

    eprintln!(
        "[fc-agent] restore phases complete (epoch={}): exec + egress ready elapsed_ms={:.3}",
        restore_epoch,
        restore_started.elapsed().as_secs_f64() * 1000.0
    );
    Ok(())
}

/// Restart systemd-journald after snapshot restore.
///
/// The journal file is corrupted because journald was mid-write when the snapshot
/// was taken. On restart, journald renames the corrupt file and creates a fresh one.
async fn restart_journald() {
    match tokio::process::Command::new("systemctl")
        .args(["restart", "systemd-journald"])
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            eprintln!("[fc-agent] journald restarted after restore");
        }
        Ok(output) => {
            eprintln!(
                "[fc-agent] WARNING: journald restart failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Err(e) => {
            eprintln!("[fc-agent] WARNING: failed to restart journald: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn exec_rebind_timeout_is_a_restore_readiness_error() {
        let done = AtomicBool::new(false);
        let notify = Notify::new();
        // A stale permit must never substitute for this restore generation's
        // authoritative completion flag.
        notify.notify_one();

        let error = wait_for_exec_rebind(&done, &notify, std::time::Duration::ZERO)
            .await
            .expect_err("missing exec re-registration must fail restore readiness");
        assert!(
            format!("{error:#}").contains("did not re-register"),
            "unexpected error: {error:#}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failed_restore_cannot_publish_output_from_warm_start() {
        let (output, writer, _reset_rx) = crate::output::create();
        drop(writer);
        let status = RestoreStatus::new(output.clone());
        status.begin().expect("initial pending restore state");

        // Put the WarmStart path on the executor while restore is still pending,
        // then publish the cleanup failure. The failed outcome must win without
        // emitting the output reconnect that the host treats as readiness.
        let waiter_status = status.clone();
        let waiter = tokio::spawn(async move { waiter_status.wait_for_output_readiness().await });
        tokio::task::yield_now().await;
        status.fail();

        let result = waiter.await.expect("WarmStart readiness task panicked");
        assert!(
            result.is_err(),
            "a failed restore must reject WarmStart output readiness"
        );
        assert!(
            !output.reconnect_requested(),
            "a failed restore must not request an output reconnect"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn successful_restore_requests_reconnect_before_warm_start_wakes() {
        let (output, writer, _reset_rx) = crate::output::create();
        drop(writer);
        let status = RestoreStatus::new(output.clone());

        let waiter_status = status.clone();
        let waiter = tokio::spawn(async move { waiter_status.wait_for_output_readiness().await });
        tokio::task::yield_now().await;
        status.succeed().expect("pending restore should succeed");

        waiter
            .await
            .expect("WarmStart readiness task panicked")
            .expect("successful restore should publish readiness");
        assert!(
            output.reconnect_requested(),
            "Succeeded must request reconnect before waking WarmStart"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cold_start_success_is_retained_without_an_existing_waiter() {
        let (output, writer, _reset_rx) = crate::output::create();
        drop(writer);
        let status = RestoreStatus::new(output.clone());

        // ColdStart completes before any WarmStart waiter subscribes. The watch
        // sender must retain Succeeded even with zero receivers, and the same
        // transition must request the ordinary cold-start reconnect.
        status.succeed().expect("pending cold start should succeed");
        assert!(output.reconnect_requested());
        status
            .wait_for_output_readiness()
            .await
            .expect("late subscriber must observe retained Succeeded");
    }
}
