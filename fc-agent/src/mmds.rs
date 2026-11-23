use anyhow::{Context, Result};
use tokio::time::{sleep, Duration};

use crate::types::{LatestMetadata, Plan};

/// Fetch the container plan from MMDS with retry.
pub async fn fetch_plan() -> Result<Plan> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    eprintln!(
        "[fc-agent] requesting MMDS V2 session token from http://169.254.169.254/latest/api/token"
    );
    let token_response = match client
        .put("http://169.254.169.254/latest/api/token")
        .header("X-metadata-token-ttl-seconds", "21600")
        .timeout(Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) => {
            eprintln!("[fc-agent] token request succeeded");
            resp
        }
        Err(e) => {
            eprintln!("[fc-agent] token request FAILED - detailed error:");
            eprintln!("[fc-agent]   error type: {:?}", e);
            if e.is_timeout() {
                eprintln!("[fc-agent]   TIMEOUT: MMDS not responding within 5 seconds");
            } else if e.is_connect() {
                eprintln!("[fc-agent]   CONNECTION ERROR: Cannot reach 169.254.169.254");
            }
            return Err(e).context("requesting MMDS session token");
        }
    };

    let token_status = token_response.status();
    eprintln!(
        "[fc-agent] token response status: {} {}",
        token_status.as_u16(),
        token_status.canonical_reason().unwrap_or("")
    );

    let token = token_response
        .text()
        .await
        .context("reading session token")?;
    eprintln!(
        "[fc-agent] got token: {} bytes ({})",
        token.len(),
        if token.is_empty() { "EMPTY!" } else { "ok" }
    );

    eprintln!("[fc-agent] fetching plan from http://169.254.169.254/latest/container-plan");
    let plan_response = match client
        .get("http://169.254.169.254/latest/container-plan")
        .header("X-metadata-token", &token)
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) => {
            eprintln!("[fc-agent] plan request succeeded");
            resp
        }
        Err(e) => {
            eprintln!("[fc-agent] plan request FAILED: {:?}", e);
            return Err(e).context("fetching from MMDS");
        }
    };

    let plan_status = plan_response.status();
    eprintln!(
        "[fc-agent] plan response status: {} {}",
        plan_status.as_u16(),
        plan_status.canonical_reason().unwrap_or("")
    );

    if !plan_status.is_success() {
        eprintln!(
            "[fc-agent] ERROR: HTTP {} - this is NOT a 2xx success code",
            plan_status.as_u16()
        );
    }

    let body = plan_response.text().await.context("reading plan body")?;
    eprintln!(
        "[fc-agent] plan response body ({} bytes): {}",
        body.len(),
        body
    );

    let plan: Plan = match serde_json::from_str(&body) {
        Ok(p) => {
            eprintln!("[fc-agent] successfully parsed JSON into Plan struct");
            p
        }
        Err(e) => {
            eprintln!("[fc-agent] JSON PARSING FAILED:");
            eprintln!("[fc-agent]   parse error: {}", e);
            eprintln!("[fc-agent]   body was: {}", body);
            return Err(e.into());
        }
    };

    Ok(plan)
}

/// Fetch `/latest` metadata with a default client (used by the boot-plan transport).
pub async fn fetch_latest_metadata_default() -> Result<LatestMetadata> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .no_proxy()
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    fetch_latest_metadata(&client).await
}

async fn fetch_latest_metadata(client: &reqwest::Client) -> Result<LatestMetadata> {
    let token_response = client
        .put("http://169.254.169.254/latest/api/token")
        .header("X-metadata-token-ttl-seconds", "21600")
        .timeout(Duration::from_millis(500))
        .send()
        .await?;
    let token = token_response.text().await?;

    let response = client
        .get("http://169.254.169.254/latest")
        .header("X-metadata-token", &token)
        .header("Accept", "application/json")
        .timeout(Duration::from_millis(500))
        .send()
        .await?;

    let body = response.text().await?;
    let metadata: LatestMetadata = serde_json::from_str(&body)?;
    Ok(metadata)
}

/// Why the restore-epoch watcher woke up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreEpochWake {
    PollInterval,
    TransportReset,
    ResetSenderClosed,
    /// The output connection was made or dropped, which changes the poll cadence.
    ArmedChanged,
}

/// Latching transport-reset wakeup plus a periodic fallback.
///
/// `watch::Receiver::changed` observes a generation published before the wait
/// future is created, which closes the check-to-park lost-wakeup window. Once
/// the sender closes, the reset arm is permanently disarmed so a perpetually
/// ready `changed()` error cannot spin the metadata fetch loop.
///
/// On Firecracker the periodic fetch is the restore event only while no transport reset can
/// reach the agent: while the output writer holds an established vsock connection
/// (`ResetWatch::armed_rx`), a restore resets it and wakes this schedule, so the fetch backs
/// off to [`Self::ARMED_POLL`]. An idle guest polled MMDS every 50 ms before, 20 local HTTP
/// requests a second through virtio-net and the VMM for the guest's whole life.
///
/// A reset wakes the watcher for one fetch and changes no interval. The host binds the
/// restore-only vsock listener, with the new epoch in its document, before it resumes the
/// guest, and a guest restored behind the snapshot boundary reads that document
/// (`RestoreMetadataControl::select_transport`), so the first fetch after the reset finds
/// the epoch. If that fetch fails, nothing retries it faster than the steady poll
/// (`poll_interval`: 50 ms over MMDS, 250 ms over Cloud Hypervisor's vsock), which applies
/// because the writer dropped its connection on the reset; the watcher logs which of the
/// two happened. The reset used to open a one-second window of fetches 2 ms apart, meant
/// for an epoch that reached MMDS only after the resume. The window closed when a restore was
/// dispatched inside it, so a reset the watcher read after it had dispatched that restore
/// (the poll, or the connection edge, had woken it first) and the reset on a resumed
/// snapshot source each ran the full second: about 500 token and document requests.
struct RestoreEpochSchedule {
    reset_rx: tokio::sync::watch::Receiver<u64>,
    reset_armed: bool,
    armed_rx: tokio::sync::watch::Receiver<bool>,
    /// The poll interval while the output connection is established; None where a
    /// transport reset is not known to reach the agent on restore (Cloud Hypervisor), or
    /// once the writer has gone.
    armed_interval: Option<Duration>,
    poll_interval: Duration,
}

impl RestoreEpochSchedule {
    /// The backstop poll while a transport reset would wake the watcher.
    const ARMED_POLL: Duration = Duration::from_secs(10);

    fn new(transport: crate::bootplan::Transport, reset: crate::output::ResetWatch) -> Self {
        let poll_interval = match transport {
            crate::bootplan::Transport::Mmds => Duration::from_millis(50),
            crate::bootplan::Transport::Vsock => Duration::from_millis(250),
        };
        // Firecracker queues the transport reset in the device state it saves, so every
        // restore of it raises one. Cloud Hypervisor keeps its poll as it was.
        let armed_interval = match transport {
            crate::bootplan::Transport::Mmds => Some(Self::ARMED_POLL),
            crate::bootplan::Transport::Vsock => None,
        };
        Self {
            reset_rx: reset.reset_rx,
            reset_armed: true,
            armed_rx: reset.armed_rx,
            armed_interval,
            poll_interval,
        }
    }

    async fn wait(&mut self) -> RestoreEpochWake {
        let armed = *self.armed_rx.borrow_and_update();
        let interval = match self.armed_interval {
            Some(armed_interval) if armed => armed_interval,
            _ => self.poll_interval,
        };

        tokio::select! {
            _ = sleep(interval) => RestoreEpochWake::PollInterval,
            changed = self.reset_rx.changed(), if self.reset_armed => {
                match changed {
                    Ok(()) => RestoreEpochWake::TransportReset,
                    Err(_) => {
                        self.reset_armed = false;
                        RestoreEpochWake::ResetSenderClosed
                    }
                }
            }
            changed = self.armed_rx.changed(), if self.armed_interval.is_some() => {
                if changed.is_err() {
                    // The writer is gone: nothing will report a reset any more.
                    self.armed_interval = None;
                }
                RestoreEpochWake::ArmedChanged
            }
        }
    }
}

/// Watch for restore-epoch changes and handle clone restore, over either transport.
///
/// Firecracker polls MMDS; Cloud Hypervisor polls the host's boot-plan vsock port
/// (#632 P2) — both via [`crate::bootplan::fetch_metadata`], so the restore handling is
/// identical. MMDS polls at 50ms (a cheap local HTTP get); vsock polls slower because each
/// poll reopens the boot-plan connection and re-reads the whole plan document.
///
/// The restore EVENT is the vsock transport reset the device raises at resume
/// (surfaced by the output writer as `vsock_reset.reset_rx`): a snapshot freezes this
/// loop mid-`sleep`, so with a poll alone a restored clone waits out the frozen sleep's
/// remainder before even looking for its epoch. On a reset event the loop fetches
/// once, at once: the restore document is on the host's vsock listener before the guest
/// resumes ([`RestoreEpochSchedule`]). While the output
/// connection that would see the reset is established, the steady poll is only a
/// backstop ([`RestoreEpochSchedule::ARMED_POLL`]); while it is not (TTY VMs, a writer
/// that has dropped its connection, Cloud Hypervisor) the loop polls as it always did.
/// The writer drops its connection on the reset it reports, so a fetch that fails right
/// after a reset is followed by the steady poll (50 ms over MMDS, 250 ms over Cloud
/// Hypervisor's vsock) until the restore has reconnected it. The loop logs whether the
/// fetch a reset asked for succeeded.
pub async fn watch_restore_epoch(
    signals: crate::restore::RestoreSignals,
    transport: crate::bootplan::Transport,
) {
    let mut last_epoch: Option<String> = None;
    let mut restore_control = RestoreMetadataControl::new(transport);

    // Track the egress proxy generation at the last stable point.
    // Updated after each handle_clone_restore completes successfully.
    let mut egress_gen_at_last_stable: Option<u64> =
        signals.egress_gen_rx.as_ref().map(|rx| *rx.borrow());

    let mut schedule = RestoreEpochSchedule::new(transport, signals.vsock_reset.clone());

    loop {
        let after_reset = schedule.wait().await == RestoreEpochWake::TransportReset;
        if after_reset {
            eprintln!("[fc-agent] vsock transport reset observed; reading the restore document");
        }

        let boundary_is_armed = crate::snapshot_network::boundary_is_armed();
        let metadata_transport = restore_control.select_transport(boundary_is_armed);
        let metadata = match crate::bootplan::fetch_metadata(metadata_transport).await {
            Ok(value) => {
                if after_reset {
                    eprintln!(
                        "[fc-agent] restore document read after the transport reset \
                         (restore_epoch={:?})",
                        value.restore_epoch
                    );
                }
                value
            }
            Err(e) => {
                if after_reset {
                    eprintln!(
                        "[fc-agent] the restore document fetch after the transport reset \
                         failed; the next is the {:?} poll: {:#}",
                        schedule.poll_interval, e
                    );
                }
                // Log the first few then every 200th failure to avoid spam while still
                // surfacing persistent errors after snapshot restore.
                static FAIL_COUNT: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let count = FAIL_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if count < 5 || count.is_multiple_of(200) {
                    eprintln!(
                        "[fc-agent] restore metadata fetch failed (count={} transport={:?} \
                         boundary_armed={} awaiting_mmds_epoch={:?}): {:#}",
                        count,
                        metadata_transport,
                        boundary_is_armed,
                        restore_control.awaiting_mmds_epoch,
                        e
                    );
                }
                continue;
            }
        };
        match restore_control.observe(
            metadata_transport,
            metadata.restore_epoch.as_deref(),
            last_epoch.as_deref(),
        ) {
            Ok(RestoreMetadataDisposition::Dispatch) => {}
            Ok(RestoreMetadataDisposition::IgnoreUntilMirror) => {
                eprintln!(
                    "[fc-agent] ignoring snapshot-old MMDS restore epoch while waiting for \
                     the host to mirror the authoritative vsock generation"
                );
                continue;
            }
            Err(error) => {
                eprintln!(
                    "[fc-agent] restore metadata rejected by control-generation gate: {error:#}"
                );
                continue;
            }
        }

        if let Some(ref current) = metadata.restore_epoch {
            match &last_epoch {
                None => {
                    eprintln!("[fc-agent] detected restore-epoch: {}", current,);
                    // Publish the handled generation in watcher state BEFORE the
                    // sequence publishes readiness: the host can request another
                    // snapshot as soon as readiness appears, and that snapshot must
                    // not capture an old `last_epoch` and mistake its existing
                    // listener for a new restore control generation.
                    last_epoch = Some(current.clone());
                    apply_restore_epoch(current, &signals, egress_gen_at_last_stable, &metadata)
                        .await;
                    // Update the stable generation only after the whole sequence
                    // succeeded.
                    egress_gen_at_last_stable =
                        signals.egress_gen_rx.as_ref().map(|rx| *rx.borrow());
                }
                Some(prev) if prev != current => {
                    eprintln!("[fc-agent] restore-epoch changed: {} -> {}", prev, current,);
                    // Publish the handled generation in watcher state BEFORE the
                    // sequence publishes readiness: the host can request another
                    // snapshot as soon as readiness appears, and that snapshot must
                    // not capture an old `last_epoch` and mistake its existing
                    // listener for a new restore control generation.
                    last_epoch = Some(current.clone());
                    apply_restore_epoch(current, &signals, egress_gen_at_last_stable, &metadata)
                        .await;
                    // Update the stable generation only after the whole sequence
                    // succeeded.
                    egress_gen_at_last_stable =
                        signals.egress_gen_rx.as_ref().map(|rx| *rx.borrow());
                }
                _ => {}
            }
        }
    }
}

/// Carry one restore generation through the whole guest-side sequence, in the
/// one order that is fail-CLOSED, and shut the clone down on any failure.
///
/// Both dispatch sites (first epoch seen, and epoch changed) run exactly this;
/// keeping one copy is what stops the ordering contract below from drifting
/// between them. Returns only on success — every failure path shuts the VM
/// down after marking the restore failed, so readiness is never published for a
/// generation that did not complete.
///
/// The order is load-bearing:
/// 1. `begin()` — the watcher owns this generation.
/// 2. `restore_flag` — makes `notify_cache_ready_and_wait` stop polling BEFORE
///    output reconnect changes vsock state under it.
/// 3. `handle_clone_restore` — rebind exec, reconnect egress.
/// 4. Host ACK BEFORE `succeed()`: `succeed()` releases the WarmStart gate in
///    `agent::run()` and starts the container, so an ACK that fails after it
///    has already let a workload touch shared volumes and external services on
///    a clone the host is about to reject — fail-OPEN inside the guest. Exec
///    does not go through `RestoreStatus`, so ACKing first costs nothing:
///    there is no container output to miss, because there is no container yet.
/// 5. `succeed()` — with the boundary acknowledged, the workload may start.
#[allow(clippy::too_many_arguments)]
async fn apply_restore_epoch(
    current: &str,
    signals: &crate::restore::RestoreSignals,
    egress_gen_at_last_stable: Option<u64>,
    metadata: &crate::types::LatestMetadata,
) {
    // A snapshot of a running VM was taken with the agent's console held quiet
    // (`--prepare-snapshot-network`), and this clone inherits that hold. Release it
    // first, so every line of this restore reaches the console.
    crate::console::release_source_snapshot_hold();
    // Test-only hold between the watcher's read of the restore document and the
    // restore sequence: a guest that stalls here, as one paging its memory in from a
    // cold file does, must still step its clock to the host's (issue 1375).
    failpoint::hit_async("restore.post_epoch_read").await;
    if let Err(error) = signals.restore_status.begin() {
        eprintln!(
            "[fc-agent] FATAL: cannot begin clone restore (epoch={}): {:#}; \
             shutting down clone",
            current, error
        );
        signals.restore_status.fail();
        crate::system::shutdown_vm(1).await;
    }
    signals
        .restore_flag
        .store(true, std::sync::atomic::Ordering::Release);
    // Before the workload's next allocation, so a clone never runs in the mode
    // its snapshot's source had when it asked for another.
    if let Some(mode) = metadata.transparent_hugepage.as_deref() {
        if let Err(error) = crate::thp::apply(mode) {
            signals.restore_status.fail();
            eprintln!(
                "[fc-agent] FATAL: clone restore failed closed (epoch={}): {:#}; \
                 shutting down clone",
                current, error
            );
            crate::system::shutdown_vm(1).await;
        }
    }
    let phases = match crate::restore::handle_clone_restore(
        signals,
        metadata,
        egress_gen_at_last_stable,
        current,
    )
    .await
    {
        Ok(phases) => phases,
        Err(error) => {
            signals.restore_status.fail();
            eprintln!(
                "[fc-agent] FATAL: clone restore failed closed (epoch={}): {:#}. \
                 Output/exec readiness will not be published; shutting down clone",
                current, error
            );
            crate::system::shutdown_vm(1).await
        }
    };
    if let Err(error) = crate::vsock::notify_restore_complete(current, &phases).await {
        signals.restore_status.fail();
        eprintln!(
            "[fc-agent] FATAL: restore-completion ACK failed closed \
             (epoch={} phase=notify-host): {:#}; host lifecycle/exec \
             readiness will not be published; shutting down clone",
            current, error
        );
        crate::system::shutdown_vm(1).await;
    }
    if let Err(error) = signals.restore_status.succeed() {
        signals.restore_status.fail();
        eprintln!(
            "[fc-agent] FATAL: cannot publish clone restore readiness \
             (epoch={}): {:#}; shutting down clone",
            current, error
        );
        crate::system::shutdown_vm(1).await;
    }
    // 6. A container that was running when the snapshot was taken is running in the
    //    clone, and the clone's host process has not been told.
    crate::container::resend_container_started_after_restore().await;
}

#[derive(Debug, PartialEq, Eq)]
enum RestoreMetadataDisposition {
    Dispatch,
    IgnoreUntilMirror,
}

/// Selects one immutable restore generation across the vsock→MMDS handoff.
///
/// Firecracker's normal control plane is MMDS, but a current snapshot contains
/// eth0 down. The host therefore publishes the new restore epoch over vsock
/// before resume and mirrors that exact epoch into MMDS afterward. VCPUs can run
/// before the MMDS PUT, so the snapshot-old MMDS epoch must not be dispatched in
/// between. The latch is installed only for a *new* vsock epoch: a source VM
/// created by an earlier restore still has its old listener, and its snapshot
/// preparation must not capture a false pending handoff.
struct RestoreMetadataControl {
    preferred: crate::bootplan::Transport,
    awaiting_mmds_epoch: Option<String>,
}

impl RestoreMetadataControl {
    fn new(preferred: crate::bootplan::Transport) -> Self {
        Self {
            preferred,
            awaiting_mmds_epoch: None,
        }
    }

    /// An armed boundary means eth0 is still down, so MMDS is unreachable and
    /// the host's restore-only vsock listener is the only control plane. Once
    /// cleanup disarms the boundary, MMDS is reachable again; whether its
    /// contents may be *dispatched* is [`Self::observe`]'s decision, not this
    /// one.
    fn select_transport(&self, boundary_is_armed: bool) -> crate::bootplan::Transport {
        use crate::bootplan::Transport;
        match self.preferred {
            Transport::Vsock => Transport::Vsock,
            Transport::Mmds if boundary_is_armed => Transport::Vsock,
            Transport::Mmds => Transport::Mmds,
        }
    }

    fn observe(
        &mut self,
        selected: crate::bootplan::Transport,
        observed_epoch: Option<&str>,
        last_dispatched_epoch: Option<&str>,
    ) -> Result<RestoreMetadataDisposition> {
        use crate::bootplan::Transport;

        if self.preferred == Transport::Vsock {
            return Ok(RestoreMetadataDisposition::Dispatch);
        }

        if selected == Transport::Vsock {
            let epoch = observed_epoch
                .context("armed Firecracker restore metadata over vsock has no restore-epoch")?;
            if Some(epoch) != last_dispatched_epoch {
                self.awaiting_mmds_epoch = Some(epoch.to_string());
            } else {
                // This is the source VM preparing another snapshot while its
                // existing restore listener still serves the already-handled
                // epoch. Never capture a pending old MMDS handoff in the new
                // snapshot; its clone's listener will provide a new epoch.
                self.awaiting_mmds_epoch = None;
            }
            return Ok(RestoreMetadataDisposition::Dispatch);
        }

        if let Some(expected) = self.awaiting_mmds_epoch.as_deref() {
            if observed_epoch == Some(expected) {
                self.awaiting_mmds_epoch = None;
                return Ok(RestoreMetadataDisposition::Dispatch);
            }
            return Ok(RestoreMetadataDisposition::IgnoreUntilMirror);
        }

        Ok(RestoreMetadataDisposition::Dispatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootplan::Transport;
    use std::future::{poll_fn, Future};
    use std::pin::Pin;
    use std::task::Poll;

    /// A reset watch whose writer is running but never connected. The armed sender is
    /// leaked so it stays open for the test: a closed one would wake the schedule.
    fn unarmed(reset_rx: tokio::sync::watch::Receiver<u64>) -> crate::output::ResetWatch {
        let (armed_tx, armed_rx) = tokio::sync::watch::channel(false);
        std::mem::forget(armed_tx);
        crate::output::ResetWatch { reset_rx, armed_rx }
    }

    #[tokio::test(start_paused = true)]
    async fn an_established_output_connection_replaces_the_50ms_mmds_poll() {
        let (reset_tx, reset_rx) = tokio::sync::watch::channel(0u64);
        let (armed_tx, armed_rx) = tokio::sync::watch::channel(true);
        let mut schedule = RestoreEpochSchedule::new(
            crate::bootplan::Transport::Mmds,
            crate::output::ResetWatch { reset_rx, armed_rx },
        );

        // Armed: no fetch for a whole second, where the old schedule fetched 20 times.
        let mut wait = Box::pin(schedule.wait());
        assert_pending_once(wait.as_mut()).await;
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_pending_once(wait.as_mut()).await;

        // A restore's transport reset still wakes it at once.
        reset_tx.send(1).unwrap();
        assert_eq!(wait.await, RestoreEpochWake::TransportReset);

        // The writer dropping its connection wakes it too, and from then the 50 ms poll
        // is back: nothing would report the next reset.
        let mut wait = Box::pin(schedule.wait());
        assert_pending_once(wait.as_mut()).await;
        armed_tx.send(false).unwrap();
        assert_eq!(wait.await, RestoreEpochWake::ArmedChanged);
        let before = tokio::time::Instant::now();
        let mut wait = Box::pin(schedule.wait());
        assert_pending_once(wait.as_mut()).await;
        tokio::time::advance(Duration::from_millis(50)).await;
        assert_eq!(wait.await, RestoreEpochWake::PollInterval);
        assert_eq!(
            tokio::time::Instant::now() - before,
            Duration::from_millis(50)
        );
    }

    /// A transport reset is one wakeup. The watcher that takes it after its restore was
    /// already dispatched (the poll or the connection edge woke it first), and the one on
    /// a resumed snapshot source, have no epoch to find, and each used to fetch every 2 ms
    /// for the next second.
    ///
    /// RED BEFORE THE FIX: the wait after the reset returned PollInterval 2 ms later.
    #[tokio::test(start_paused = true)]
    async fn a_transport_reset_is_one_fetch_and_no_fast_poll() {
        for transport in [
            crate::bootplan::Transport::Mmds,
            crate::bootplan::Transport::Vsock,
        ] {
            let steady = match transport {
                crate::bootplan::Transport::Mmds => Duration::from_millis(50),
                crate::bootplan::Transport::Vsock => Duration::from_millis(250),
            };
            let (reset_tx, reset_rx) = tokio::sync::watch::channel(0u64);
            let mut schedule = RestoreEpochSchedule::new(transport, unarmed(reset_rx));
            reset_tx.send(1).unwrap();
            assert_eq!(schedule.wait().await, RestoreEpochWake::TransportReset);

            let before = tokio::time::Instant::now();
            let mut wait = Box::pin(schedule.wait());
            assert_pending_once(wait.as_mut()).await;
            tokio::time::advance(steady - Duration::from_millis(1)).await;
            assert_pending_once(wait.as_mut()).await;
            tokio::time::advance(Duration::from_millis(1)).await;
            assert_eq!(wait.await, RestoreEpochWake::PollInterval);
            assert_eq!(tokio::time::Instant::now() - before, steady);
        }
    }

    /// With the output connection established, a reset leaves the 10 s backstop in place.
    #[tokio::test(start_paused = true)]
    async fn a_transport_reset_leaves_the_armed_backstop() {
        let (reset_tx, reset_rx) = tokio::sync::watch::channel(0u64);
        let (_armed_tx, armed_rx) = tokio::sync::watch::channel(true);
        let mut schedule = RestoreEpochSchedule::new(
            crate::bootplan::Transport::Mmds,
            crate::output::ResetWatch { reset_rx, armed_rx },
        );
        reset_tx.send(1).unwrap();
        assert_eq!(schedule.wait().await, RestoreEpochWake::TransportReset);

        let mut wait = Box::pin(schedule.wait());
        assert_pending_once(wait.as_mut()).await;
        tokio::time::advance(RestoreEpochSchedule::ARMED_POLL - Duration::from_millis(1)).await;
        assert_pending_once(wait.as_mut()).await;
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(wait.await, RestoreEpochWake::PollInterval);
    }

    #[tokio::test(start_paused = true)]
    async fn cloud_hypervisor_keeps_its_poll_with_the_output_connected() {
        let (_reset_tx, reset_rx) = tokio::sync::watch::channel(0u64);
        let (_armed_tx, armed_rx) = tokio::sync::watch::channel(true);
        let mut schedule = RestoreEpochSchedule::new(
            crate::bootplan::Transport::Vsock,
            crate::output::ResetWatch { reset_rx, armed_rx },
        );
        let before = tokio::time::Instant::now();
        assert_eq!(schedule.wait().await, RestoreEpochWake::PollInterval);
        assert_eq!(
            tokio::time::Instant::now() - before,
            Duration::from_millis(250)
        );
    }

    async fn assert_pending_once<F>(mut future: Pin<&mut F>)
    where
        F: Future,
    {
        poll_fn(|cx| match future.as_mut().poll(cx) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("future completed before its signal"),
        })
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn transport_reset_generation_published_before_park_wakes_immediately() {
        let (reset_tx, reset_rx) = tokio::sync::watch::channel(0u64);
        let mut schedule =
            RestoreEpochSchedule::new(crate::bootplan::Transport::Mmds, unarmed(reset_rx));

        // Publish before `wait()` creates its `changed()` future. A one-shot
        // notification would lose this edge; the retained watch generation must
        // make the subsequent wait immediately ready.
        reset_tx.send(1).expect("reset receiver remains alive");
        let before = tokio::time::Instant::now();

        assert_eq!(schedule.wait().await, RestoreEpochWake::TransportReset);
        assert_eq!(
            tokio::time::Instant::now(),
            before,
            "fallback clock advanced"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn closed_reset_sender_is_disarmed_instead_of_spinning() {
        let (reset_tx, reset_rx) = tokio::sync::watch::channel(0u64);
        let mut schedule =
            RestoreEpochSchedule::new(crate::bootplan::Transport::Mmds, unarmed(reset_rx));
        drop(reset_tx);

        assert_eq!(schedule.wait().await, RestoreEpochWake::ResetSenderClosed);

        // `changed()` on a closed channel remains immediately ready forever.
        // The second wait must use only the 50ms fallback, proving the closed
        // arm was permanently removed from the select.
        let mut next = Box::pin(schedule.wait());
        assert_pending_once(next.as_mut()).await;
        tokio::time::advance(Duration::from_millis(49)).await;
        assert_pending_once(next.as_mut()).await;
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(next.await, RestoreEpochWake::PollInterval);
    }

    #[tokio::test(start_paused = true)]
    async fn missing_transport_reset_uses_periodic_fallback() {
        let (_reset_tx, reset_rx) = tokio::sync::watch::channel(0u64);
        let mut schedule =
            RestoreEpochSchedule::new(crate::bootplan::Transport::Mmds, unarmed(reset_rx));
        let before = tokio::time::Instant::now();

        let mut wait = Box::pin(schedule.wait());
        assert_pending_once(wait.as_mut()).await;
        tokio::time::advance(Duration::from_millis(49)).await;
        assert_pending_once(wait.as_mut()).await;
        tokio::time::advance(Duration::from_millis(1)).await;

        assert_eq!(wait.await, RestoreEpochWake::PollInterval);
        assert_eq!(
            tokio::time::Instant::now() - before,
            Duration::from_millis(50)
        );
    }

    #[test]
    fn firecracker_restore_latches_vsock_epoch_until_exact_mmds_mirror() {
        let mut control = RestoreMetadataControl::new(Transport::Mmds);

        assert_eq!(control.select_transport(true), Transport::Vsock);
        assert_eq!(
            control
                .observe(Transport::Vsock, Some("epoch-a"), None)
                .unwrap(),
            RestoreMetadataDisposition::Dispatch
        );

        // Cleanup has removed the boundary manifest, but the host has not yet
        // run after resume to replace snapshot-old MMDS epoch-b.
        assert_eq!(control.select_transport(false), Transport::Mmds);
        assert_eq!(
            control
                .observe(Transport::Mmds, Some("epoch-b"), Some("epoch-a"))
                .unwrap(),
            RestoreMetadataDisposition::IgnoreUntilMirror
        );

        // Only the exact mirror acknowledges the handoff. A genuinely later
        // MMDS generation can then be dispatched normally.
        assert_eq!(
            control
                .observe(Transport::Mmds, Some("epoch-a"), Some("epoch-a"))
                .unwrap(),
            RestoreMetadataDisposition::Dispatch
        );
        assert_eq!(control.select_transport(false), Transport::Mmds);
        assert_eq!(
            control
                .observe(Transport::Mmds, Some("epoch-c"), Some("epoch-a"))
                .unwrap(),
            RestoreMetadataDisposition::Dispatch
        );
    }

    #[test]
    fn later_armed_restore_replaces_a_captured_pending_handoff() {
        let mut control = RestoreMetadataControl::new(Transport::Mmds);
        control.awaiting_mmds_epoch = Some("epoch-a".to_string());

        assert_eq!(
            control.select_transport(true),
            Transport::Vsock,
            "an armed restore must supersede an MMDS handoff captured in memory"
        );
        assert_eq!(
            control
                .observe(Transport::Vsock, Some("epoch-c"), Some("epoch-a"))
                .unwrap(),
            RestoreMetadataDisposition::Dispatch
        );
        assert_eq!(control.awaiting_mmds_epoch.as_deref(), Some("epoch-c"));
    }

    #[test]
    fn source_snapshot_does_not_latch_its_existing_restore_listener() {
        let mut control = RestoreMetadataControl::new(Transport::Mmds);
        control.awaiting_mmds_epoch = Some("epoch-a".to_string());
        assert_eq!(
            control
                .observe(Transport::Vsock, Some("epoch-a"), Some("epoch-a"))
                .unwrap(),
            RestoreMetadataDisposition::Dispatch
        );
        assert_eq!(control.awaiting_mmds_epoch, None);
    }
}
