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
    /// VIRTIO_VSOCK_EVENT_TRANSPORT_RESET. The VMM queues that event at
    /// snapshot SAVE, so it fires on a restored clone AND on a resumed source
    /// (which is why it is only a wakeup, never a classification). The epoch
    /// watcher uses it to fast-poll for a new restore-epoch instead of
    /// finishing a frozen 50ms sleep; on a resumed source the fast poll finds
    /// no new epoch and lapses. Accelerator only: the normal poll cadence
    /// remains the correctness path.
    pub vsock_reset_rx: watch::Receiver<u64>,
    /// NFS mounts from the boot-time plan, kept for post-restore remounting.
    /// MMDS can't be re-fetched here: the host's restore-epoch PUT replaces the
    /// whole MMDS store, so `container-plan` is gone by the time restore runs.
    /// This cache lives in the snapshot's memory image, so a restored VM sees
    /// exactly the mounts that were active when the snapshot was taken.
    pub nfs_mounts: Vec<crate::types::NfsMount>,
}

/// Per-phase milliseconds for one guest-side restore, carried to the host in
/// the restore-completion ACK frame so every clone's critical path is
/// attributed in the host's own logs (guest serial output is not reliably
/// captured for clones).
#[derive(Debug, Default, serde::Serialize)]
pub struct RestorePhases {
    pub clock_ms: f64,
    pub ipv6_ms: f64,
    pub tcp_cleanup_ms: f64,
    /// Wire copy of
    /// [`crate::snapshot_network::RestoreNetworkReport::verified_armed`]:
    /// whether the boundary took the verified fast path.
    pub tcp_verified: bool,
    /// Boundary sub-phases, straight from
    /// [`crate::snapshot_network::RestoreNetworkReport`].
    pub tcp_verify_ms: f64,
    pub tcp_reassert_ms: f64,
    pub tcp_destroy_ms: f64,
    pub tcp_reopen_ms: f64,
    pub neighbor_ms: f64,
    pub nfs_ms: f64,
    pub exec_wait_ms: f64,
    pub egress_wait_ms: f64,
    pub total_ms: f64,
}

impl RestorePhases {
    /// Compact single-line JSON for the ACK frame.
    pub fn to_frame_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }
}

fn elapsed_ms(since: std::time::Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
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
/// The exec rebind is REQUESTED first (it only touches vsock state, never the
/// external link the TCP boundary is manipulating) and AWAITED after the
/// network phases, so the rebind proceeds concurrently with the boundary work
/// instead of serializing behind it. Chrony re-convergence and the journald
/// restart have no bearing on the readiness contract, so both run in the
/// background, off the host's ACK path.
///
/// FUSE volumes are NOT remounted here. The reconnectable multiplexer
/// detects the dead vsock and auto-reconnects to the clone's VolumeServer.
/// The kernel FUSE session stays alive — processes see a brief hang, not errors.
///
/// `clone_ipv6`: For routed mode, the unique per-clone IPv6 that replaces the
/// snapshot's shared guest IPv6 on eth0. Without this, all clones share the same
/// IPv6 and return traffic gets ECMP-routed to the wrong clone.
///
/// `host_time`: epoch seconds already fetched with the restore epoch by the
/// metadata watcher. Re-fetching the metadata document here would repeat the
/// transport round trip milliseconds after the watcher completed it, for a
/// value with one-second resolution.
pub async fn handle_clone_restore(
    signals: &RestoreSignals,
    clone_ipv6: Option<&str>,
    egress_gen_before: Option<u64>,
    restore_epoch: &str,
    host_time: &str,
) -> anyhow::Result<RestorePhases> {
    let restore_started = std::time::Instant::now();
    let mut phases = RestorePhases::default();
    eprintln!("[fc-agent] handling restore (epoch={})", restore_epoch);

    // Request the exec re-register immediately (AsyncFd epoll is stale after the
    // transport reset). Reset confirmation flag, then signal. Set flag BEFORE
    // notify to prevent a race where select! drops the Notified future (see
    // exec.rs doc comment). The wait happens after the network phases below.
    signals.exec_rebind_done.store(false, Ordering::Release);
    signals.exec_rebind_needed.store(true, Ordering::Release);
    signals.exec_rebind.notify_one();

    // Sync clock — snapshot restore leaves the VM clock frozen at snapshot time.
    // Services that validate timestamps (auth, TLS, sessions) will fail with
    // stale time. The step is soft-fail by construction (chronyd converges
    // eventually), so nothing here branches on it.
    let clock_started = std::time::Instant::now();
    crate::bootplan::set_system_clock(host_time);
    // Reset chrony after the clock jump so it doesn't lose its sources:
    // `makestep` forces it to accept the stepped time. Its convergence is
    // not part of the readiness contract (the clock is already stepped),
    // so it must not hold the host's ACK.
    spawn_restore_side_job(async {
        let _ = tokio::process::Command::new("chronyc")
            .args(["makestep"])
            .output()
            .await;
    });
    phases.clock_ms = elapsed_ms(clock_started);

    // Restart journald in the background, started this early so it usually
    // finishes before the ACK without ever holding it. The journal file was
    // mid-write when the snapshot was taken, so the restored journald finds a
    // corrupted file and gets stuck; systemd's watchdog would kill it after
    // 3 min. Nothing in the readiness contract reads journald (container
    // output travels over vsock), so a synchronous restart, which is a full
    // systemd job, would only add latency. After the clock step so the fresh
    // journal opens with correct timestamps, and registered so the shutdown
    // path can settle it rather than race it.
    spawn_restore_side_job(restart_journald());

    // Reconfigure IPv6 — before any network traffic can use the old address.
    // The link is still down here (the snapshot captured it down), so the new
    // address sits tentative until the boundary republishes the link.
    let ipv6_started = std::time::Instant::now();
    if let Some(new_ipv6) = clone_ipv6 {
        network::reconfigure_ipv6(new_ipv6).await;
    }
    phases.ipv6_ms = elapsed_ms(ipv6_started);

    let tcp_cleanup_started = std::time::Instant::now();
    eprintln!(
        "[fc-agent] restore phase=tcp-cleanup epoch={} begin",
        restore_epoch
    );
    let boundary = crate::snapshot_network::restore_snapshot_network()
        .await
        .context("restore phase tcp-cleanup")?;
    phases.tcp_cleanup_ms = elapsed_ms(tcp_cleanup_started);
    phases.tcp_verified = boundary.verified_armed;
    phases.tcp_verify_ms = boundary.verify_ms;
    phases.tcp_reassert_ms = boundary.reassert_ms;
    phases.tcp_destroy_ms = boundary.destroy_ms;
    phases.tcp_reopen_ms = boundary.reopen_ms;
    eprintln!(
        "[fc-agent] restore phase=tcp-cleanup epoch={} complete elapsed_ms={:.3}",
        restore_epoch, phases.tcp_cleanup_ms
    );

    // Do not flush the neighbor table. A client can already be using an entry
    // while restore cleanup runs, and `ip neigh flush all` has no generation
    // boundary. One broadcast ARP request refreshes the gateway and teaches the
    // new bridge/pasta path without deleting unrelated/current neighbors.
    let neighbor_started = std::time::Instant::now();
    network::refresh_gateway_arp();
    phases.neighbor_ms = elapsed_ms(neighbor_started);

    // Remount NFS shares: their kernel TCP connections to the host's NFS
    // server died with the snapshot transport reset, and a hard NFS mount
    // wedges every accessor until remounted. Lazy-unmount then mount fresh
    // re-establishes the connection against the host's re-created export.
    // Uses the boot-time plan cached in RestoreSignals — see its doc comment
    // for why MMDS can't be consulted here.
    let nfs_started = std::time::Instant::now();
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
    phases.nfs_ms = elapsed_ms(nfs_started);

    // Await the exec re-register requested at the top and the egress proxy
    // reconnect together: the two are independent (vsock listener rebind
    // versus the proxy's own reconnect loop), so the ACK owes the slower of
    // the two, never their sum.
    //
    // Exec: this ensures accept() works before the host can reach the exec
    // server. Wait on the AtomicBool (the source of truth) and use the Notify
    // only as a wakeup: a stale stored permit (e.g. left over from a previous
    // restore whose wait timed out before the rebind finished) must not let
    // this restore proceed before its own re-register has completed. The flag
    // was reset above, so it only reads true once the exec server has
    // re-registered for THIS restore.
    //
    // Egress: no explicit signal needed — the proxy detects the dead vsock fd
    // natively via Interest::ERROR (EPOLLERR fires instantly after transport
    // reset), its session exits, and the reconnect loop connects a new vsock.
    // If it already reconnected, wait_for returns immediately (watch retains
    // the latest value).
    let exec_wait = async {
        let started = std::time::Instant::now();
        wait_for_exec_rebind(
            &signals.exec_rebind_done,
            &signals.exec_rebind_done_notify,
            std::time::Duration::from_secs(5),
        )
        .await
        .context("waiting for exec server re-registration after restore")?;
        Ok::<f64, anyhow::Error>(elapsed_ms(started))
    };
    let egress_wait = async {
        let started = std::time::Instant::now();
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
        Ok::<f64, anyhow::Error>(elapsed_ms(started))
    };
    let (exec_wait_ms, egress_wait_ms) = tokio::try_join!(exec_wait, egress_wait)?;
    phases.exec_wait_ms = exec_wait_ms;
    phases.egress_wait_ms = egress_wait_ms;
    eprintln!("[fc-agent] exec re-registered after restore");

    phases.total_ms = elapsed_ms(restore_started);
    eprintln!(
        "[fc-agent] restore phases complete (epoch={}): exec + egress ready elapsed_ms={:.3}",
        restore_epoch, phases.total_ms
    );
    Ok(phases)
}

/// Background jobs a restore started that must not outlive the VM.
///
/// These run off the readiness path (nothing in the readiness contract reads
/// journald or waits for chrony to converge), but each drives a system
/// service, and one still running when the VM is torn down races the
/// shutdown itself: the guest asks systemd to power off while systemd is
/// mid-restart of one of its own units. Keeping the handles lets the
/// shutdown path settle them first, exactly as `agent::run` already settles
/// the chronyd setup task.
static PENDING_RESTORE_JOBS: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>> =
    std::sync::Mutex::new(Vec::new());

/// Run a restore side-job in the background, registered for settling.
fn spawn_restore_side_job(job: impl std::future::Future<Output = ()> + Send + 'static) {
    PENDING_RESTORE_JOBS.lock().unwrap().push(tokio::spawn(job));
}

/// Wait, bounded, for a background journald restart to finish.
///
/// Called on the shutdown path so a systemd job this process started cannot
/// still be running while the guest powers the VM off. The bound is not a
/// guess about the job's duration: it caps how long a wedged systemd can
/// hold the VM open, which is the same trade `agent::run` makes for chronyd.
pub async fn settle_restore_side_jobs(timeout: std::time::Duration) {
    settle_pending_jobs(&PENDING_RESTORE_JOBS, timeout).await
}

/// The settle itself, over whichever slot holds the jobs. Taking the slot as
/// an argument keeps the behaviour testable without the process-wide static,
/// which parallel tests would otherwise share. The timeout is the budget for
/// the whole set, so a wedged job cannot multiply the shutdown delay by the
/// number of jobs.
async fn settle_pending_jobs(
    slot: &std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    timeout: std::time::Duration,
) {
    let jobs = std::mem::take(&mut *slot.lock().unwrap());
    if jobs.is_empty() {
        return;
    }
    let deadline = tokio::time::Instant::now() + timeout;
    for job in jobs {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, job).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => eprintln!("[fc-agent] restore side-job panicked: {error}"),
            Err(_) => {
                eprintln!(
                    "[fc-agent] restore side-jobs still running after {timeout:?} at \
                     shutdown; proceeding without them"
                );
                return;
            }
        }
    }
}

/// Restart systemd-journald after snapshot restore.
///
/// The journal file is corrupted because journald was mid-write when the snapshot
/// was taken. On restart, journald renames the corrupt file and creates a fresh one.
///
/// This runs in the background (measured at 32-51ms on an idle guest, more on a
/// freshly restored one where the unit's pages fault back in) and the shutdown
/// path settles it, so it can neither hold the ACK nor race the VM going away.
///
/// A snapshot taken immediately after the ACK can still capture the unit
/// mid-restart. That is deliberate rather than unhandled: the guest's snapshot
/// boundary runs as a separate one-shot process, so there is no task for it to
/// join, and the condition is self-correcting because every restore of the
/// resulting image restarts journald again. Trading a cross-process handshake
/// for a mid-write journal that the next restore repairs anyway is not worth
/// the coupling.
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
mod restore_side_job_tests {
    use super::*;

    fn slot(
        handles: Vec<tokio::task::JoinHandle<()>>,
    ) -> std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>> {
        std::sync::Mutex::new(handles)
    }

    /// Background jobs a restore started must not still be running when the
    /// guest powers the VM off: the shutdown path settles them first. With
    /// the jobs merely detached, this observes them unfinished.
    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_settles_every_restore_started_background_job() {
        let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let jobs = (0..3)
            .map(|index| {
                let counter = done.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(10 * (index + 1))).await;
                    counter.fetch_add(1, Ordering::AcqRel);
                })
            })
            .collect();
        let pending = slot(jobs);

        settle_pending_jobs(&pending, std::time::Duration::from_secs(5)).await;

        assert_eq!(
            done.load(Ordering::Acquire),
            3,
            "shutdown proceeded while a restore's system jobs were still running"
        );
        assert!(
            pending.lock().unwrap().is_empty(),
            "settled jobs must not be waited on twice"
        );
    }

    /// The settle is bounded for the SET, not per job: one wedged job cannot
    /// multiply the delay, and cannot hold the VM open indefinitely.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_wedged_background_job_cannot_hold_the_vm_open() {
        let pending = slot(vec![
            tokio::spawn(std::future::pending::<()>()),
            tokio::spawn(std::future::pending::<()>()),
        ]);

        let before = tokio::time::Instant::now();
        settle_pending_jobs(&pending, std::time::Duration::from_secs(5)).await;
        assert_eq!(
            tokio::time::Instant::now() - before,
            std::time::Duration::from_secs(5),
            "the settle must give up at its bound, once for the whole set"
        );
    }

    /// No restore, nothing registered: shutdown pays nothing.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn shutdown_without_a_restore_waits_for_nothing() {
        let pending = slot(Vec::new());
        let before = tokio::time::Instant::now();
        settle_pending_jobs(&pending, std::time::Duration::from_secs(5)).await;
        assert_eq!(tokio::time::Instant::now(), before);
    }

    /// The real registration path hands the shutdown path its jobs.
    #[tokio::test(flavor = "current_thread")]
    async fn the_restore_path_registers_its_jobs_for_settling() {
        spawn_restore_side_job(async {});
        assert!(
            !PENDING_RESTORE_JOBS.lock().unwrap().is_empty(),
            "a restore must leave its system jobs where shutdown can find them"
        );
        settle_restore_side_jobs(std::time::Duration::from_secs(5)).await;
        assert!(PENDING_RESTORE_JOBS.lock().unwrap().is_empty());
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
