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
    /// The NFS shares fc-agent mounted at boot, each with the ID of its mount, kept for
    /// post-restore remounting. Set once, right after the boot mounted them, which comes
    /// before the cache-ready handshake and so before any snapshot. MMDS can't be
    /// re-fetched here: the host's restore-epoch PUT replaces the whole MMDS store, so
    /// `container-plan` is gone by the time restore runs. This cache lives in the
    /// snapshot's memory image, so a restored VM sees exactly the mounts that were active
    /// when the snapshot was taken.
    /// A restore that remounts them records the new mounts' IDs here, so the next restore of a
    /// snapshot of this clone finds them.
    pub nfs_mounts: crate::mounts::NfsMountRecords,
}

/// Per-phase milliseconds for one guest-side restore, carried to the host in
/// the restore-completion ACK frame so every clone's critical path is
/// attributed in the host's own logs (guest serial output is not reliably
/// captured for clones).
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct RestorePhases {
    pub clock_ms: f64,
    pub ipv6_ms: f64,
    pub tcp_cleanup_ms: f64,
    /// The parts of `tcp_cleanup_ms` outside the boundary's own sub-timers
    /// (#1090): the begin line written to the guest console (timed here), and
    /// from the boundary's report the lock wait, the constructors, the gate
    /// close with the UDP rule, and the cleanup-complete console line.
    pub tcp_begin_console_ms: f64,
    pub tcp_lock_wait_ms: f64,
    pub tcp_construct_ms: f64,
    pub tcp_gate_arm_ms: f64,
    pub tcp_complete_console_ms: f64,
    /// What `tcp_cleanup_ms` holds beyond every part above and the boundary's
    /// verify, barrier, egress open and destroy timers. Nonzero means a step
    /// of the phase is still unaccounted.
    pub tcp_cleanup_untimed_ms: f64,
    /// Wire copy of
    /// [`crate::snapshot_network::RestoreNetworkReport::verified_armed`]:
    /// whether the boundary took the verified fast path.
    pub tcp_verified: bool,
    /// Boundary sub-phases, straight from
    /// [`crate::snapshot_network::RestoreNetworkReport`].
    pub tcp_verify_ms: f64,
    pub tcp_reassert_ms: f64,
    pub tcp_barrier_ms: f64,
    pub tcp_destroy_ms: f64,
    pub tcp_reopen_ms: f64,
    /// Milliseconds opening the gate's egress half after the socket retirement.
    pub tcp_egress_open_ms: f64,
    /// The restore hook's run at publication, with the egress half open and the
    /// ingress half still gated; zero when the plan names none.
    pub tcp_hook_ms: f64,
    /// The cleanup's counts, from the same report: sockets destroyed, sockets
    /// the kernel had already retired, and the SOCK_DESTROY datagrams sent.
    pub tcp_destroyed: usize,
    pub tcp_already_gone: usize,
    pub tcp_destroy_datagrams: usize,
    /// Per-operation gate timings from the same report: each family's
    /// verification read and UDP rule (one `iptables` spawn each), and for
    /// each half of the gate the nf_tables read of its rules and the one
    /// commit that removes it in both families.
    /// The gate was verified and the UDP rule added over nfnetlink.
    pub tcp_gate_netlink: bool,
    pub tcp_gate_verify_ipv4_ms: f64,
    pub tcp_gate_verify_ipv6_ms: f64,
    pub tcp_gate_udp_ipv4_ms: f64,
    pub tcp_gate_udp_ipv6_ms: f64,
    pub tcp_gate_egress_read_ms: f64,
    pub tcp_gate_egress_commit_ms: f64,
    pub tcp_gate_ingress_read_ms: f64,
    pub tcp_gate_ingress_commit_ms: f64,
    pub neighbor_ms: f64,
    /// The inherited neighbours' flush and the pins that follow it.
    pub neighbour_pin_ms: f64,
    pub nfs_ms: f64,
    pub exec_wait_ms: f64,
    pub egress_wait_ms: f64,
    pub total_ms: f64,
    /// How long the host took to answer the connection the ACK is sent on (#1080). Set by
    /// `vsock::notify_restore_complete` once the connection is answered.
    pub ack_connect_ms: f64,
    /// GPU attach (rescan, driver load, CDI spec), from the start of the
    /// restore sequence; it overlaps the network phases. Zero without GPUs.
    pub gpu_ms: f64,
    /// GPUs the driver initialised.
    pub gpus: usize,
    /// Each attach step's milliseconds; absent without GPUs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu_steps: Option<crate::gpu::AttachSteps>,
}

impl RestorePhases {
    /// Copy what the snapshot network boundary reported into the record.
    pub(crate) fn record_boundary(
        &mut self,
        boundary: &crate::snapshot_network::RestoreNetworkReport,
    ) {
        self.tcp_lock_wait_ms = boundary.lock_wait_ms;
        self.tcp_construct_ms = boundary.construct_ms;
        self.tcp_gate_arm_ms = boundary.gate_arm_ms;
        // A routed clone's move to its own address runs inside the boundary.
        self.ipv6_ms = boundary.before_egress_ms;
        self.tcp_complete_console_ms = boundary.complete_console_ms;
        self.tcp_verified = boundary.verified_armed;
        self.tcp_verify_ms = boundary.verify_ms;
        self.tcp_reassert_ms = boundary.reassert_ms;
        self.tcp_barrier_ms = boundary.barrier_ms;
        self.tcp_destroy_ms = boundary.destroy_ms;
        self.tcp_reopen_ms = boundary.reopen_ms;
        self.tcp_egress_open_ms = boundary.egress_open_ms;
        self.tcp_hook_ms = boundary.hook_ms;
        self.tcp_destroyed = boundary.tally.destroyed;
        self.tcp_already_gone = boundary.tally.already_gone;
        self.tcp_destroy_datagrams = boundary.tally.datagrams;
        let gate = boundary.gate;
        self.tcp_gate_netlink = gate.netlink;
        self.tcp_gate_verify_ipv4_ms = gate.verify_ipv4_ms;
        self.tcp_gate_verify_ipv6_ms = gate.verify_ipv6_ms;
        self.tcp_gate_udp_ipv4_ms = gate.udp_ipv4_ms;
        self.tcp_gate_udp_ipv6_ms = gate.udp_ipv6_ms;
        self.tcp_gate_egress_read_ms = gate.egress_read_ms;
        self.tcp_gate_egress_commit_ms = gate.egress_commit_ms;
        self.tcp_gate_ingress_read_ms = gate.ingress_read_ms;
        self.tcp_gate_ingress_commit_ms = gate.ingress_commit_ms;
        // Clamped: each timer is rounded on its own clock, so their sum can
        // pass the phase's by a few microseconds.
        self.tcp_cleanup_untimed_ms =
            (self.tcp_cleanup_ms - self.tcp_cleanup_accounted_ms()).max(0.0);
    }

    /// The milliseconds of `tcp_cleanup_ms` the disjoint timers account for.
    /// `tcp_reassert_ms` is left out because it contains the gate close, the
    /// UDP rule and the barrier, each of which has its own timer.
    pub(crate) fn tcp_cleanup_accounted_ms(&self) -> f64 {
        self.tcp_begin_console_ms
            + self.tcp_lock_wait_ms
            + self.tcp_construct_ms
            + self.tcp_verify_ms
            + self.tcp_gate_arm_ms
            + self.tcp_barrier_ms
            + self.ipv6_ms
            + self.tcp_egress_open_ms
            + self.tcp_destroy_ms
            + self.tcp_complete_console_ms
    }

    /// Compact single-line JSON for the ACK frame, every timing rounded to
    /// the microsecond. Unrounded, `as_secs_f64() * 1000.0` prints up to 17
    /// significant digits, and a record of such values passed the frame's
    /// 1024 bytes, which drops the whole record.
    pub fn to_frame_json(&self) -> String {
        let Ok(mut value) = serde_json::to_value(self) else {
            return "{}".to_string();
        };
        round_timings(&mut value);
        serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
    }
}

/// Rounds every floating-point timing to the microsecond, in nested objects too
/// (`gpu_steps`), so a 17-significant-digit `as_secs_f64() * 1000.0` cannot push
/// the ACK past its frame budget.
fn round_timings(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(fields) => fields.values_mut().for_each(round_timings),
        field if field.is_f64() => {
            if let Some(ms) = field.as_f64() {
                *field = serde_json::json!((ms * 1000.0).round() / 1000.0);
            }
        }
        _ => {}
    }
}

pub(crate) fn elapsed_ms(since: std::time::Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

/// The shares a restore remounts: the snapshot's, at the other of the two server ports
/// (2049 and `NFS_RESTORE_PORT`), so the remount gets an NFS client of its own.
pub(crate) fn nfs_remounts(shares: &[crate::types::NfsMount]) -> Vec<crate::types::NfsMount> {
    shares
        .iter()
        .map(|share| crate::types::NfsMount {
            port: match share.port {
                Some(crate::types::NFS_RESTORE_PORT) => None,
                _ => Some(crate::types::NFS_RESTORE_PORT),
            },
            ..share.clone()
        })
        .collect()
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
/// `metadata` is the restore document. Its fields, unpacked below:
///
/// `clone_ipv6`: For routed mode, the unique per-clone IPv6 that replaces the
/// snapshot's shared guest IPv6 on eth0. Without this, all clones share the same
/// IPv6 and return traffic gets ECMP-routed to the wrong clone.
///
/// `host_time`: the host's clock in nanoseconds, fetched with the restore epoch by the
/// metadata watcher. The step reads the host's clock again instead
/// (`bootplan::sync_clock_from_host`) and uses this value only when that read fails,
/// adding the time since this function started. The time between the host's stamp
/// and that start is not counted: a restore that pages in from a cold memory file
/// stalled long enough there to leave a clone 130 ms behind the host for 9 minutes
/// (issue 1375).
///
/// `identity`: the restoring host's identity address and resolvers, which replace
/// the preparing host's before egress opens.
pub async fn handle_clone_restore(
    signals: &RestoreSignals,
    metadata: &crate::types::LatestMetadata,
    egress_gen_before: Option<u64>,
    restore_epoch: &str,
) -> anyhow::Result<RestorePhases> {
    let clone_ipv6 = metadata.clone_ipv6.as_deref();
    let host_time = metadata.host_time_ns.as_str();
    let restore_hook = metadata.restore_hook.as_deref();
    let gpus = metadata.gpus.as_slice();
    let identity = &metadata.identity;
    let restore_started = std::time::Instant::now();
    let mut phases = RestorePhases::default();
    eprintln!("[fc-agent] handling restore (epoch={})", restore_epoch);

    // The kernel's attributes of the --map volumes' files are the source's, with the rest of
    // their timeout to run here. Dropped now, the first access asks the server, and a file
    // the host rewrote since the snapshot keeping its size and mtime is read afresh (#1049).
    // The call waits until the kernel has them, so the restore hook and publication below
    // never read through the old attributes.
    let expired = tokio::task::spawn_blocking(|| {
        fuse_pipe::client::expire_cached_attributes(std::time::Duration::from_secs(5))
    })
    .await
    .unwrap_or(0);
    eprintln!("[fc-agent] restore: dropped the cached attributes of {expired} mapped files");

    // The chronyd inherited from the snapshot must be gone before the clock step
    // below, which it would not notice (`agent::retire_chronyd_for_restore`).
    // Started now so the kill overlaps the network phases; awaited at the step.
    let chronyd_retired = tokio::spawn(crate::agent::retire_chronyd_for_restore());

    // GPUs the host attached before resuming: rescan, load the driver and
    // write the CDI spec. Started first and awaited before publication, so it
    // overlaps the network phases (it touches no network state) and a failure
    // keeps the clone unpublished and unacknowledged. The container starts
    // only after the acknowledgement, so it always sees the GPUs.
    let gpu_attach = (!gpus.is_empty()).then(|| {
        let gpus = gpus.to_vec();
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            let report = crate::gpu::attach(&gpus).await?;
            Ok::<_, anyhow::Error>((elapsed_ms(started), report))
        })
    });

    // Request the exec re-register immediately (AsyncFd epoll is stale after the
    // transport reset). Reset confirmation flag, then signal. Set flag BEFORE
    // notify to prevent a race where select! drops the Notified future (see
    // exec.rs doc comment). The wait happens after the network phases below.
    signals.exec_rebind_done.store(false, Ordering::Release);
    signals.exec_rebind_needed.store(true, Ordering::Release);
    signals.exec_rebind.notify_one();

    // Pin the neighbour entry BEFORE the boundary reopens the gate. The clone
    // inherits the source's neighbour table, and restore_snapshot_network ends
    // by publishing; anything answered between publication and a later pin
    // could still go to the inherited MAC, which is the exact SYN-ACK/RST
    // failure this boundary work exists to remove. Safe to do early: nothing
    // purges the neighbour table any more.
    // Order matters: drop the inherited entries first, then pin the one the
    // host's health checks depend on. Flushing after the pin would delete it.
    //
    // The flush also runs before the IPv6 swap below. The swap announces the
    // new address, which lets in the traffic that was waiting for it, and the
    // guest resolves its senders' addresses to answer; a flush running then
    // deletes those entries mid-resolution and left one INCOMPLETE for a
    // second (2 of 1700 routed restores with an early client). Pinned by
    // `inherited_neighbours_are_flushed_before_the_new_ipv6_is_announced`.
    let neighbour_pin_started = std::time::Instant::now();
    network::flush_stale_neighbours().await;
    network::pin_namespace_neighbour().await;
    // A routed clone's IPv6 gateway is pinned too, before the swap below: a
    // resolution of it in flight across the swap lost its answer to the
    // deleted snapshot-time address and held every reply for a second
    // (#1160). Pinned by `the_routed_gateway_is_pinned_before_the_ipv6_swap`.
    if clone_ipv6.is_some() {
        network::pin_routed_gateway_neighbour().await;
    }
    phases.neighbour_pin_ms = elapsed_ms(neighbour_pin_started);

    // Take the restoring host's identity: its host identity address on lo, and
    // its resolvers. The guest booted with the preparing host's, and on another
    // host both are wrong. Done here, before the boundary opens the gate's egress
    // half and before the clock step: every daemon that reconnects once egress
    // opens, and the restore hook that reloads daemons, must find the new
    // identity, never the old. The new address is added before the old one is
    // deleted, so lo always holds one. A failure keeps the clone unpublished.
    let identity_started = std::time::Instant::now();
    network::apply_host_identity(identity.host_ipv6.as_deref())
        .await
        .context("restore phase host-identity")?;
    if let Some(ref dns) = identity.dns {
        network::apply_restore_dns(dns).context("restore phase host-identity resolvers")?;
    }
    eprintln!(
        "[fc-agent] restore phase=host-identity epoch={restore_epoch} elapsed_ms={:.3}",
        elapsed_ms(identity_started)
    );
    // ANNOUNCE, as early as possible. The host cannot forward a packet to this
    // clone until its namespace has learned the guest's MAC, and it only learns
    // that when the guest transmits. The old boundary got this for free: taking
    // the link down and up made the guest announce itself the moment it came
    // back. With the link never cycled -- deliberately, because cycling it
    // purged routes and neighbours -- nothing was announcing until this probe
    // ran at the END of the restore sequence, and fcvm's post-restore port
    // verification could time out first ("guest 10.0.2.100 never appeared at L2"
    // while the namespace's own table showed it REACHABLE moments later).
    //
    // One broadcast ARP costs nothing and teaches the whole segment. It refreshes
    // the gateway without flushing the neighbour table, whose entries a client
    // can already be using while the cleanup runs.
    let neighbor_started = std::time::Instant::now();
    network::refresh_gateway_arp();
    phases.neighbor_ms = elapsed_ms(neighbor_started);

    let tcp_cleanup_started = std::time::Instant::now();
    // The console is a serial device and a write to it can block, so the line
    // is timed like the rest of the phase.
    let begin_console_started = std::time::Instant::now();
    eprintln!(
        "[fc-agent] restore phase=tcp-cleanup epoch={} begin",
        restore_epoch
    );
    phases.tcp_begin_console_ms = elapsed_ms(begin_console_started);
    // The boundary retires the snapshot-time sockets and opens the gate's
    // egress half; ingress stays gated until the publication below. The
    // boundary lock is held across everything in between.
    //
    // A routed clone moves to its own IPv6 address inside the boundary, once the
    // clone's UDP rule is in and before egress opens (#1114). The new address is
    // what lets the network reach a routed clone, so taken before the UDP rule it
    // let datagrams reach the restored workload, and nothing may leave from the
    // snapshot's address once egress is open. The link is UP throughout, and the
    // gate holds new inbound connections off until publication.
    // What the retired-address step carried, for the reconcile after the hook and the
    // watch after publication.
    let carried_listeners: std::sync::Mutex<Option<crate::retired_address::Carried>> =
        std::sync::Mutex::new(None);
    let move_to_clone_address = || async {
        let Some(new_ipv6) = clone_ipv6 else {
            return Ok(());
        };
        let retired = network::reconfigure_ipv6(new_ipv6).await;
        // A listener bound to the address just retired, or to an earlier clone
        // generation's, stays reachable at the new one (#1083). Before the
        // publication below, so no client finds it refused; a failure keeps the
        // clone unpublished rather than publishing one whose service is gone.
        let current: std::net::Ipv6Addr = new_ipv6
            .parse()
            .with_context(|| format!("the clone's IPv6 address {new_ipv6:?}"))?;
        let carried = tokio::task::spawn_blocking(move || {
            crate::retired_address::carry_listeners(retired, current)
        })
        .await
        .context("joining the retired-address step")?
        .context("keeping listeners on a retired IPv6 address reachable")?;
        if !carried.plan.translate.is_empty() {
            eprintln!(
                "[fc-agent] listeners on retired IPv6 address(es) {:?} answer on {current}: {:?}",
                carried.plan.keep, carried.plan.translate
            );
        }
        *carried_listeners.lock().unwrap() = Some(carried);
        Ok(())
    };
    let restored = crate::snapshot_network::restore_snapshot_network(move_to_clone_address)
        .await
        .context("restore phase tcp-cleanup")?;
    phases.tcp_cleanup_ms = elapsed_ms(tcp_cleanup_started);
    eprintln!(
        "[fc-agent] restore phase=tcp-cleanup epoch={} complete elapsed_ms={:.3} \
         destroyed={} already_gone={}",
        restore_epoch,
        phases.tcp_cleanup_ms,
        restored.report().tally.destroyed,
        restored.report().tally.already_gone
    );

    // Sync clock — snapshot restore leaves the VM clock frozen at snapshot time.
    // Services that validate timestamps (auth, TLS, sessions) will fail with
    // stale time. The step is soft-fail by construction (chronyd converges
    // eventually), so nothing here branches on it.
    //
    // Stepped here, after the boundary has opened the gate's egress half: the
    // jump makes every daemon whose lease ran out in it reconnect at once, and a
    // connect the gated egress answers with a reset is a definitive refusal to
    // ServiceRouter (.claude/CLAUDE.md, "SUSPENDING A VM DOES NOT DISTURB IT").
    // The NFS remount and the hook run after it. The host's clock is read here,
    // right before the step, not taken from the restore document the watcher read
    // before this function started (see `host_time` above).
    let clock_started = std::time::Instant::now();
    let chronyd_retired = chronyd_retired.await;
    if let Err(error) = crate::bootplan::sync_clock_from_host().await {
        eprintln!(
            "[fc-agent] WARNING: reading the host's clock for the restore step failed: \
             {error:#}; stepping to the restore document's"
        );
        crate::bootplan::set_system_clock(host_time, restore_started.elapsed());
    }
    // A fresh chronyd, whose every measurement is taken against the stepped
    // clock. Its convergence is not part of the readiness contract (the clock
    // is already stepped), so it must not hold the host's ACK.
    match chronyd_retired {
        Ok(retired) => spawn_restore_side_job(crate::agent::restart_chronyd_after_restore(
            retired,
            restore_epoch.to_string(),
        )),
        Err(e) => eprintln!("[fc-agent] ERROR: retiring the snapshot's chronyd panicked: {e}"),
    }
    phases.clock_ms = elapsed_ms(clock_started);

    // Restart journald in the background without ever holding the ACK. The
    // journal file was mid-write when the snapshot was taken, so the restored
    // journald finds a corrupted file and gets stuck; systemd's watchdog would
    // kill it after 3 min. Nothing in the readiness contract reads journald
    // (container output travels over vsock), so a synchronous restart, which is
    // a full systemd job, would only add latency. After the clock step so the
    // fresh journal opens with correct timestamps, and registered so the
    // shutdown path can settle it rather than race it.
    spawn_restore_side_job(restart_journald());

    // Remount NFS shares: their kernel TCP connections to the host's NFS
    // server died with the snapshot transport reset, and a hard NFS mount
    // wedges every accessor until remounted. Lazy-unmount then mount fresh
    // re-establishes the connection against the host's re-created export.
    // Uses the boot-time plan cached in RestoreSignals — see its doc comment
    // for why MMDS can't be consulted here.
    let nfs_started = std::time::Instant::now();
    let mounted_nfs = signals
        .nfs_mounts
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .context("restored before the boot recorded its NFS mounts")?;
    if !mounted_nfs.is_empty() {
        eprintln!(
            "[fc-agent] remounting {} NFS share(s) after restore",
            mounted_nfs.len()
        );
        // Each share is remounted at the mount point of the mount the boot made, found by
        // its mount ID, not by resolving its guest path again (#1086).
        let mountinfo = tokio::fs::read_to_string("/proc/self/mountinfo")
            .await
            .context("reading /proc/self/mountinfo to remount NFS shares after restore")?;
        let mut shares = Vec::with_capacity(mounted_nfs.len());
        for mounted in &mounted_nfs {
            let point = crate::mounts::nfs_remount_point(&mountinfo, mounted)
                .context("remounting NFS shares after restore, so the clone stays unpublished")?;
            let _ = tokio::process::Command::new("umount")
                .args(["-l", &point])
                .output()
                .await;
            shares.push(crate::types::NfsMount {
                mount_path: point,
                ..mounted.share.clone()
            });
        }
        // This guest is a copy of the VM the snapshot was taken of, NFSv4 client
        // identity included. A twin clone, or that VM while the server still holds its
        // state, uses the same identity from another address, and the server refuses
        // the second mount ("Operation not permitted") or never answers it. Mount under
        // an identity of this clone's own. The identity reaches only a new NFS client,
        // and the kernel reuses its client for a server address it already holds: a
        // file the restored workload holds open keeps the snapshot's client alive
        // through the lazy unmount. So the shares are remounted at the other server
        // port (`nfs_remounts`), which every clone namespace forwards to the host's nfsd,
        // as it does the port the snapshot's mounts use.
        if let Err(error) = crate::mounts::fresh_nfs_client_identity() {
            eprintln!("[fc-agent] WARNING: could not give the clone its own NFS client identity: {error:#}");
        }
        // No mount record: the boot's check passed the mounts this restore reproduces.
        // A failure fails the restore closed: the hook below may use the share,
        // and a mount point that is an ordinary directory would take its writes;
        // a published clone whose share is missing is the stale state the hook
        // exists to prevent.
        let shares = nfs_remounts(&shares);
        let remounted_paths = crate::mounts::mount_nfs_shares(&shares, None)
            .context("remounting NFS shares after restore, so the clone stays unpublished")?;
        // The remount made new mounts. Their IDs replace the boot's, so a snapshot of this
        // clone, restored, remounts these and not the ones this restore unmounted.
        let remounted = crate::mounts::record_nfs_mounts(&shares)
            .context("recording the NFS shares remounted after restore")?;
        *signals.nfs_mounts.lock().unwrap_or_else(|e| e.into_inner()) = Some(remounted);
        // The container bound the snapshot's mounts when it started; give it these.
        for (pid, path) in crate::mounts::nfs_mounts_in_other_namespaces(&remounted_paths)? {
            crate::mounts::rebind_into_namespace(pid, &path).context(
                "giving the container the remounted NFS share, so the clone stays unpublished",
            )?;
            eprintln!("[fc-agent] NFS share {path} remounted in the mount namespace of {pid}");
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

    if let Some(job) = gpu_attach {
        let (gpu_ms, report) = job
            .await
            .context("joining the GPU attach")?
            .context("restore phase gpu-attach")?;
        phases.gpu_ms = gpu_ms;
        phases.gpus = report.gpus;
        phases.gpu_steps = Some(report.steps);
        eprintln!(
            "[fc-agent] restore phase=gpu-attach epoch={restore_epoch} gpus={} elapsed_ms={gpu_ms:.3}",
            phases.gpus
        );
    }

    // Publication, last: the plan's restore hook runs now, with egress open,
    // the NFS shares remounted and the egress proxy reconnected, so whatever
    // it reaches for (a loopback admin port, a share, the network) is there;
    // then the gate's ingress half opens and a published-port client can
    // reach the clone. Whether the hook has a container to run in is the
    // container's state at this moment, asked of podman, not a marker: a
    // marker stored before podman started was wrong for a snapshot taken in
    // between, in either order.
    // After the hook and before ingress opens, the translations follow what the hook
    // left: a service it restarted on the clone's own address is reached there.
    let reconcile_after_hook = || {
        let mut carried = carried_listeners.lock().unwrap();
        if let Some(carried) = carried.as_mut() {
            if carried.keeps_watching() {
                crate::retired_address::reconcile(carried)
                    .context("keeping listeners on a retired IPv6 address reachable")?;
            }
        }
        Ok(())
    };
    let boundary = restored
        .publish(restore_hook, reconcile_after_hook)
        .await
        .context("restore phase publish")?;
    phases.record_boundary(&boundary);
    // And after publication, for as long as any translation remains.
    if let Some(carried) = carried_listeners.lock().unwrap().take() {
        crate::retired_address::watch(carried);
    }

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

/// The source of `handle_clone_restore` alone, for the tests that pin the order
/// of its steps. Bounded at the function's closing brace, so a needle that also
/// appears in a test's own string literals cannot match there.
#[cfg(test)]
pub(crate) fn handle_clone_restore_source() -> &'static str {
    let source = include_str!("restore.rs");
    let start = source
        .find("pub async fn handle_clone_restore(")
        .expect("handle_clone_restore is in restore.rs");
    let len = source[start..]
        .find("\n}\n")
        .expect("handle_clone_restore has a closing brace")
        + 2;
    &source[start..start + len]
}

#[cfg(test)]
mod restore_map_attribute_tests {
    /// A restore drops the attributes the kernel holds for the mapped files before anything
    /// else, so a clone reads a file the host rewrote since the snapshot (#1049). Without it
    /// test_read_only_map_reads_a_rewrite_that_keeps_size_and_mtime read the old bytes in a
    /// clone that looked within the attribute timeout it inherited.
    #[test]
    fn a_restore_drops_the_mapped_files_cached_attributes() {
        let restore = super::handle_clone_restore_source();
        let expire = restore
            .find("fuse_pipe::client::expire_cached_attributes(")
            .expect("handle_clone_restore keeps the source's attributes of the mapped files");
        let first_await = restore.find(".await").unwrap();
        assert!(
            expire < first_await,
            "it runs before the restore's first await"
        );
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

    /// #1009: a restore remounts its NFS shares at the other server port, and a
    /// restore of a clone of a clone goes back to the first.
    #[test]
    fn nfs_shares_are_remounted_at_the_other_server_port() {
        let shares = vec![crate::types::NfsMount {
            host_ip: "172.30.0.1".to_string(),
            host_path: "/srv/share".to_string(),
            mount_path: "/mnt/share".to_string(),
            read_only: true,
            port: None,
        }];
        let moved = nfs_remounts(&shares);
        assert_eq!(moved[0].port, Some(crate::types::NFS_RESTORE_PORT));
        assert_eq!(moved[0].host_ip, "172.30.0.1");
        assert_eq!(moved[0].host_path, "/srv/share");
        assert_eq!(moved[0].mount_path, "/mnt/share");
        assert!(moved[0].read_only);
        assert_eq!(nfs_remounts(&moved)[0].port, None);
    }

    /// The ACK telemetry, with every phase at a large value and the ACK connect time, still
    /// fits the frame budget beside a UUID epoch, so `restore_complete_frame` keeps it.
    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn the_ack_telemetry_fits_the_frame_budget() {
        let mut phases = RestorePhases::default();
        phases.clock_ms = 99_999.999;
        phases.ipv6_ms = 99_999.999;
        phases.tcp_cleanup_ms = 99_999.999;
        phases.tcp_begin_console_ms = 99_999.999;
        phases.tcp_lock_wait_ms = 99_999.999;
        phases.tcp_construct_ms = 99_999.999;
        phases.tcp_gate_arm_ms = 99_999.999;
        phases.tcp_complete_console_ms = 99_999.999;
        phases.tcp_cleanup_untimed_ms = 99_999.999;
        phases.tcp_verified = true;
        phases.tcp_verify_ms = 99_999.999;
        phases.tcp_reassert_ms = 99_999.999;
        phases.tcp_destroy_ms = 99_999.999;
        phases.tcp_reopen_ms = 99_999.999;
        phases.tcp_egress_open_ms = 99_999.999;
        phases.tcp_hook_ms = 99_999.999;
        phases.tcp_destroyed = 999_999;
        phases.tcp_already_gone = 999_999;
        phases.tcp_destroy_datagrams = 999_999;
        phases.tcp_gate_verify_ipv4_ms = 99_999.999;
        phases.tcp_gate_verify_ipv6_ms = 99_999.999;
        phases.tcp_gate_udp_ipv4_ms = 99_999.999;
        phases.tcp_gate_udp_ipv6_ms = 99_999.999;
        phases.tcp_gate_egress_read_ms = 99_999.999;
        phases.tcp_gate_egress_commit_ms = 99_999.999;
        phases.tcp_gate_ingress_read_ms = 99_999.999;
        phases.tcp_gate_ingress_commit_ms = 99_999.999;
        phases.neighbor_ms = 99_999.999;
        phases.nfs_ms = 99_999.999;
        phases.exec_wait_ms = 99_999.999;
        phases.egress_wait_ms = 99_999.999;
        phases.total_ms = 99_999.999;
        phases.ack_connect_ms = 99_999.999;
        phases.gpu_ms = 99_999.999;
        phases.gpus = 8;
        phases.gpu_steps = Some(crate::gpu::AttachSteps {
            scrub_ms: 99_999.999,
            ..Default::default()
        });
        let frame = exec_proto::restore_complete_frame(
            "8f4b2c1e-6d3a-4f5b-9c7e-0a1b2c3d4e5f",
            &phases.to_frame_json(),
        );
        assert!(
            frame.contains("\"ack_connect_ms\":99999.999")
                && frame.contains("\"scrub_ms\":99999.999"),
            "the ACK telemetry was dropped from the frame: {frame}"
        );
    }

    /// The timings are `as_secs_f64() * 1000.0`, which prints with up to 17
    /// significant digits (`0.000032999999999999996` was seen in a real ACK).
    /// With every field that long the record passed 1024 bytes and the frame
    /// dropped all of it for `{}`.
    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn long_decimal_timings_still_fit_the_frame_budget() {
        let long = 10_000.000_001_999_999_f64;
        let mut phases = RestorePhases::default();
        phases.clock_ms = long;
        phases.ipv6_ms = long;
        phases.tcp_cleanup_ms = long;
        phases.tcp_begin_console_ms = long;
        phases.tcp_lock_wait_ms = long;
        phases.tcp_construct_ms = long;
        phases.tcp_gate_arm_ms = long;
        phases.tcp_complete_console_ms = long;
        phases.tcp_cleanup_untimed_ms = long;
        phases.tcp_verified = true;
        phases.tcp_verify_ms = long;
        phases.tcp_reassert_ms = long;
        phases.tcp_destroy_ms = long;
        phases.tcp_reopen_ms = long;
        phases.tcp_egress_open_ms = long;
        phases.tcp_hook_ms = long;
        phases.tcp_destroyed = 999_999;
        phases.tcp_already_gone = 999_999;
        phases.tcp_destroy_datagrams = 999_999;
        phases.tcp_gate_verify_ipv4_ms = long;
        phases.tcp_gate_verify_ipv6_ms = long;
        phases.tcp_gate_udp_ipv4_ms = long;
        phases.tcp_gate_udp_ipv6_ms = long;
        phases.tcp_gate_egress_read_ms = long;
        phases.tcp_gate_egress_commit_ms = long;
        phases.tcp_gate_ingress_read_ms = long;
        phases.tcp_gate_ingress_commit_ms = long;
        phases.neighbor_ms = long;
        phases.nfs_ms = long;
        phases.exec_wait_ms = long;
        phases.egress_wait_ms = long;
        phases.total_ms = long;
        phases.ack_connect_ms = long;
        phases.gpu_ms = long;
        phases.gpus = 999_999;
        let frame = exec_proto::restore_complete_frame(
            "8f4b2c1e-6d3a-4f5b-9c7e-0a1b2c3d4e5f",
            &phases.to_frame_json(),
        );
        assert!(
            frame.contains("\"ack_connect_ms\":10000.0"),
            "the ACK telemetry was dropped from the frame ({} bytes): {frame}",
            frame.len()
        );
    }

    fn boundary_report(
        lock_wait_ms: f64,
        construct_ms: f64,
        complete_console_ms: f64,
    ) -> crate::snapshot_network::RestoreNetworkReport {
        crate::snapshot_network::RestoreNetworkReport {
            verified_armed: true,
            lock_wait_ms,
            construct_ms,
            complete_console_ms,
            gate_arm_ms: 4.0,
            before_egress_ms: 5.0,
            verify_ms: 50.0,
            reassert_ms: 0.0,
            barrier_ms: 20.0,
            destroy_ms: 600.0,
            egress_open_ms: 30.0,
            hook_ms: 0.0,
            reopen_ms: 0.0,
            tally: Default::default(),
            gate: Default::default(),
        }
    }

    /// #1090: `tcp_cleanup_untimed_ms` is what the phase holds beyond every
    /// disjoint timer, so a step nobody timed shows up as a number instead of
    /// hiding in `tcp_cleanup_ms`. `tcp_reassert_ms` contains the gate close,
    /// the UDP rule and the barrier, so it is not counted a second time.
    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn the_cleanup_remainder_is_what_the_timers_leave() {
        let mut phases = RestorePhases::default();
        phases.tcp_cleanup_ms = 1000.0;
        phases.tcp_begin_console_ms = 1.0;
        phases.record_boundary(&boundary_report(200.0, 3.0, 2.0));
        // 1 + 200 + 3 + 50 (verify) + 4 (gate arm) + 20 (barrier) + 5 (the move
        // to the clone's IPv6 address) + 30 (egress) + 600 (destroy) + 2 = 915.
        assert_eq!(phases.tcp_cleanup_accounted_ms(), 915.0);
        assert_eq!(phases.tcp_cleanup_untimed_ms, 85.0);
        assert_eq!(phases.ipv6_ms, 5.0);

        let mut reasserted = boundary_report(200.0, 3.0, 2.0);
        reasserted.verified_armed = false;
        reasserted.reassert_ms = 24.0;
        phases.record_boundary(&reasserted);
        assert_eq!(
            phases.tcp_cleanup_untimed_ms, 85.0,
            "the reassert contains the gate arm and the barrier and adds nothing"
        );

        // Rounding in the timers must never print a negative remainder.
        phases.tcp_cleanup_ms = 900.0;
        phases.record_boundary(&boundary_report(200.0, 3.0, 2.0));
        assert_eq!(phases.tcp_cleanup_untimed_ms, 0.0);
    }

    /// The begin line is written to a serial console that can block, so the
    /// phase times it. `handle_clone_restore` needs a whole guest, so the
    /// contract is pinned at the source.
    #[test]
    fn the_begin_console_write_is_timed_inside_the_cleanup_phase() {
        // handle_clone_restore alone, so no needle can match this test's own text.
        let source = handle_clone_restore_source();
        let started = source.find("let tcp_cleanup_started").unwrap();
        let begin = source.find("let begin_console_started").unwrap();
        let line = source[begin..]
            .find("restore phase=tcp-cleanup epoch={} begin")
            .unwrap()
            + begin;
        let stored = source
            .find("phases.tcp_begin_console_ms = elapsed_ms(begin_console_started)")
            .unwrap();
        let boundary = source
            .find("crate::snapshot_network::restore_snapshot_network(")
            .expect("handle_clone_restore runs the boundary");
        assert!(
            started < begin && begin < line && line < stored && stored < boundary,
            "the begin console write must be timed after the phase starts and before the boundary"
        );
    }

    /// The ACK names the GPU memory scrub's duration, and says nothing about
    /// GPU steps without GPUs.
    #[test]
    fn the_ack_carries_the_gpu_scrub_time() {
        let mut phases = RestorePhases::default();
        let bare: serde_json::Value = serde_json::from_str(&phases.to_frame_json()).unwrap();
        assert!(bare.get("gpu_steps").is_none(), "{bare}");
        phases.gpu_steps = Some(crate::gpu::AttachSteps {
            scrub_ms: 61.5,
            ..Default::default()
        });
        let frame: serde_json::Value = serde_json::from_str(&phases.to_frame_json()).unwrap();
        assert_eq!(frame["gpu_steps"]["scrub_ms"], 61.5, "{frame}");
    }

    /// A GPU restore's ACK with every phase (the firewall gate's included) and every
    /// GPU attach step (the scrub's included) at a 17-significant-digit timing, as
    /// measured timings often are (`79.74971599999999`), still fits the frame budget
    /// beside a UUID epoch, so the host gets the gate and GPU step timings rather than
    /// an empty `{}`. The nested step timings are rounded like the top-level ones.
    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn a_gpu_restore_ack_keeps_every_gate_and_gpu_step_timing_within_the_frame_budget() {
        let long = 12_345.678_901_234_567_f64;
        assert_eq!(format!("{long}").len(), 18, "{long}");
        let mut phases = RestorePhases::default();
        phases.clock_ms = long;
        phases.ipv6_ms = long;
        phases.tcp_cleanup_ms = long;
        phases.tcp_begin_console_ms = long;
        phases.tcp_lock_wait_ms = long;
        phases.tcp_construct_ms = long;
        phases.tcp_gate_arm_ms = long;
        phases.tcp_complete_console_ms = long;
        phases.tcp_cleanup_untimed_ms = long;
        phases.tcp_verified = true;
        phases.tcp_verify_ms = long;
        phases.tcp_reassert_ms = long;
        phases.tcp_destroy_ms = long;
        phases.tcp_reopen_ms = long;
        phases.tcp_egress_open_ms = long;
        phases.tcp_hook_ms = long;
        phases.tcp_destroyed = 999_999;
        phases.tcp_already_gone = 999_999;
        phases.tcp_destroy_datagrams = 999_999;
        phases.tcp_gate_verify_ipv4_ms = long;
        phases.tcp_gate_verify_ipv6_ms = long;
        phases.tcp_gate_udp_ipv4_ms = long;
        phases.tcp_gate_udp_ipv6_ms = long;
        phases.tcp_gate_egress_read_ms = long;
        phases.tcp_gate_egress_commit_ms = long;
        phases.tcp_gate_ingress_read_ms = long;
        phases.tcp_gate_ingress_commit_ms = long;
        phases.neighbor_ms = long;
        phases.nfs_ms = long;
        phases.exec_wait_ms = long;
        phases.egress_wait_ms = long;
        phases.total_ms = long;
        phases.ack_connect_ms = long;
        phases.gpu_ms = long;
        phases.gpus = 8;
        phases.gpu_steps = Some(crate::gpu::AttachSteps {
            check_ms: long,
            rescan_ms: long,
            nvidia_ms: long,
            uvm_ms: long,
            nodes_ms: long,
            persistenced_ms: long,
            scrub_ms: long,
            smi_ms: long,
            cdi_ms: long,
            clocks_ms: long,
        });
        let telemetry = phases.to_frame_json();
        let frame =
            exec_proto::restore_complete_frame("8f4b2c1e-6d3a-4f5b-9c7e-0a1b2c3d4e5f", &telemetry);
        assert!(
            frame.contains("\"tcp_gate_ingress_commit_ms\":12345.679")
                && frame.contains("\"scrub_ms\":12345.679")
                && frame.contains("\"persistenced_ms\":12345.679"),
            "the gate or GPU step timings were dropped from the ACK ({} bytes of telemetry): {frame}",
            telemetry.len()
        );
    }

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

    /// A restored clone must ANNOUNCE itself before the boundary publishes.
    ///
    /// The host cannot forward a packet to a clone until its namespace has
    /// learned the guest's MAC, and a namespace learns that only when the guest
    /// transmits. The old snapshot boundary got this for free: it cycled the
    /// link, and coming back up made the guest announce itself immediately.
    /// That cycle was removed because it also purged the device's routes and
    /// neighbours -- the defect this whole change exists to fix.
    ///
    /// With nothing announcing, the only transmission was the gateway ARP probe
    /// at the END of the restore sequence, and fcvm's post-restore port
    /// verification could time out first. CI caught it as
    ///
    ///   Error: port forwarding verification failed after snapshot restore:
    ///          guest 10.0.2.100 never appeared at L2 within ...
    ///
    /// while the namespace's own table showed `10.0.2.100 ... REACHABLE` in the
    /// forensics dumped moments later. The guest was not missing; it was late.
    ///
    /// The chaos fuzz found that, but only by landing inside the window. This
    /// pins the ordering that makes the window impossible, so a regression
    /// fails here every time rather than one run in N.
    /// The container starts when the restore is acknowledged, and the host
    /// publishes the clone then, so the GPU must be usable before either. The
    /// attach is joined, with its error propagated, before the boundary
    /// publishes and therefore before `apply_restore_epoch` sends the ACK.
    /// The ACK's phase record names each GPU attach step, so a slow attach is
    /// attributed to its step in the host's log; without GPUs it says nothing.
    #[test]
    fn the_ack_carries_each_gpu_attach_step() {
        let mut phases = RestorePhases::default();
        let bare: serde_json::Value = serde_json::from_str(&phases.to_frame_json()).unwrap();
        assert!(bare.get("gpu_steps").is_none(), "{bare}");

        phases.gpu_steps = Some(crate::gpu::AttachSteps {
            rescan_ms: 1.5,
            persistenced_ms: 900.0,
            ..Default::default()
        });
        let frame: serde_json::Value = serde_json::from_str(&phases.to_frame_json()).unwrap();
        let steps = &frame["gpu_steps"];
        assert_eq!(steps["rescan_ms"], 1.5, "{frame}");
        assert_eq!(steps["persistenced_ms"], 900.0, "{frame}");
        for step in [
            "check_ms",
            "nvidia_ms",
            "uvm_ms",
            "nodes_ms",
            "smi_ms",
            "cdi_ms",
            "clocks_ms",
        ] {
            assert!(steps.get(step).is_some(), "{step} missing from {frame}");
        }
    }

    /// The clone's chronyd is replaced around the clock step: the snapshot's
    /// daemon is killed (and awaited) before the step and a fresh one starts
    /// after it, and nothing asks the snapshot's daemon for its leftover
    /// correction (`chronyc makestep`), which is what stepped a clone 2.19 s off
    /// the host after the restore had set it right.
    #[test]
    fn the_snapshots_chronyd_is_gone_before_the_clock_step_and_a_fresh_one_starts_after() {
        let body = handle_clone_restore_source();
        let retire = body
            .find("tokio::spawn(crate::agent::retire_chronyd_for_restore())")
            .expect("the restore no longer kills the snapshot's chronyd");
        let retired = body
            .find("let chronyd_retired = chronyd_retired.await;")
            .expect("the restore no longer waits for the snapshot's chronyd to be gone");
        let clock = body
            .find("crate::bootplan::sync_clock_from_host()")
            .expect("the restore path no longer steps the clock");
        let restart = body
            .find(
                "crate::agent::restart_chronyd_after_restore(\n            retired,\n            \
                 restore_epoch.to_string(),",
            )
            .expect("the restore no longer starts a fresh chronyd that names its restore");
        assert!(
            retire < retired && retired < clock && clock < restart,
            "retire={retire} retired={retired} clock={clock} restart={restart}"
        );
        assert!(
            !body.contains("\"makestep\""),
            "the restore asks the snapshot's chronyd to apply its leftover correction"
        );
    }

    #[test]
    fn the_clock_steps_after_the_egress_gate_opens_and_before_publication() {
        let body = handle_clone_restore_source();
        let boundary = body
            .find("crate::snapshot_network::restore_snapshot_network(")
            .expect("the restore path no longer runs the snapshot boundary");
        let clock = body
            .find("crate::bootplan::sync_clock_from_host()")
            .expect("the restore path no longer steps the clock");
        let nfs = body
            .find("mount_nfs_shares(")
            .expect("the restore path no longer remounts NFS");
        let publish = body
            .find(".publish(restore_hook,")
            .expect("the restore path no longer publishes");
        assert!(
            clock < nfs,
            "the NFS remount must see the stepped clock: clock={clock} nfs={nfs}"
        );
        assert!(
            boundary < clock && clock < publish,
            "the restore must step the clock after the boundary has opened the gate's egress \
             half and before it publishes: a step taken while egress is gated makes every daemon \
             whose lease ran out in the jump reconnect into the gate, whose reset ServiceRouter \
             takes as a definitive refusal. boundary={boundary} clock={clock} publish={publish}"
        );
    }

    /// The restoring host's identity is in place before the boundary opens the
    /// gate's egress half, before the clock step and before the restore hook:
    /// the daemons that reconnect once egress opens, and the hook that reloads
    /// daemons, must find the new identity, never the preparing host's.
    #[test]
    fn the_host_identity_is_replaced_before_egress_opens_and_before_the_hook() {
        let body = handle_clone_restore_source();
        let identity = body
            .find("network::apply_host_identity(identity.host_ipv6.as_deref())")
            .expect("the restore path no longer replaces the host identity address");
        let fail_closed = body
            .find(".context(\"restore phase host-identity\")?")
            .expect("the restore path no longer fails closed on a host identity error");
        let dns = body
            .find("network::apply_restore_dns(dns).context(\"restore phase host-identity resolvers\")?")
            .expect("the restore path no longer gives the guest the restoring host's resolvers, or no longer fails closed when it cannot");
        let boundary = body
            .find("crate::snapshot_network::restore_snapshot_network(")
            .expect("the restore path no longer runs the snapshot boundary");
        let clock = body
            .find("crate::bootplan::sync_clock_from_host()")
            .expect("the restore path no longer steps the clock");
        let publish = body
            .find(".publish(restore_hook,")
            .expect("the restore path no longer publishes");
        assert!(
            identity < fail_closed
                && fail_closed < boundary
                && dns < boundary
                && boundary < clock
                && clock < publish,
            "the host identity and resolvers must be replaced before the boundary opens \
             egress, the clock steps and the hook runs: identity={identity} \
             fail_closed={fail_closed} dns={dns} boundary={boundary} clock={clock} \
             publish={publish}"
        );
    }

    #[test]
    fn gpu_attach_completes_before_the_clone_is_published() {
        let body = handle_clone_restore_source();
        let start = body
            .find("crate::gpu::attach(&gpus)")
            .expect("the restore path no longer attaches GPUs");
        let join = body
            .find(".context(\"restore phase gpu-attach\")?")
            .expect("the restore path no longer fails closed on a GPU attach error");
        let publish = body
            .find(".publish(restore_hook,")
            .expect("the restore path no longer publishes");
        assert!(
            start < join && join < publish,
            "the GPU attach must start, then be joined with its error propagated, before \
             the clone publishes: start={start} join={join} publish={publish}"
        );
    }

    #[test]
    fn the_clone_announces_itself_before_the_boundary_publishes() {
        let source = handle_clone_restore_source();
        let announce = source
            .find("network::refresh_gateway_arp()")
            .expect("the restore path no longer announces the clone at all");
        let publish = source
            .find("crate::snapshot_network::restore_snapshot_network(")
            .expect("the restore path no longer runs the snapshot boundary");
        assert!(
            announce < publish,
            "the clone announces itself AFTER the boundary publishes. The host \
             learns a clone's MAC only when the guest transmits, so publishing \
             first opens a window where fcvm's post-restore verification can \
             time out with \"guest never appeared at L2\" on a guest that is \
             perfectly healthy. Move network::refresh_gateway_arp() above \
             restore_snapshot_network(...)."
        );

        // Flushing inherited entries after pinning would delete the pin, and
        // announcing before flushing would teach the segment and then discard
        // what we learned in return.
        let flush = source
            .find("network::flush_stale_neighbours()")
            .expect("the restore path no longer flushes inherited neighbours");
        let pin = source
            .find("network::pin_namespace_neighbour()")
            .expect("the restore path no longer pins the health-check neighbour");
        assert!(
            flush < pin && pin < announce,
            "restore must flush inherited neighbours, then pin the health-check \
             entry, then announce. Got flush={flush} pin={pin} announce={announce}"
        );
    }
}

#[cfg(test)]
mod nfs_remount_record_tests {
    /// A restore remounts each NFS share as a new mount, and records the new mounts' IDs in
    /// place of the boot's after the mount: a snapshot of the restored clone, restored again,
    /// looks the shares up by ID and must find these, not the ones the first restore
    /// unmounted. Pinned by source order, since the mounts need a live NFS server.
    #[test]
    fn a_restore_records_the_nfs_mounts_it_made() {
        let body = super::handle_clone_restore_source();
        let squeezed: String = body.split_whitespace().collect();
        let mounted = squeezed
            .find("crate::mounts::mount_nfs_shares(&shares,None)")
            .expect("the restore does not remount the NFS shares");
        let recorded = squeezed
            .find("crate::mounts::record_nfs_mounts(&shares)")
            .expect("the restore does not record the mounts it made");
        let stored = squeezed
            .find("*signals.nfs_mounts.lock().unwrap_or_else(|e|e.into_inner())=Some(remounted);")
            .expect("the restore does not store the new records");
        assert!(mounted < recorded && recorded < stored);
    }
}
