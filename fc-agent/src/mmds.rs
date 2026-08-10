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
}

/// Latching transport-reset wakeup plus the original periodic fallback.
///
/// `watch::Receiver::changed` observes a generation published before the wait
/// future is created, which closes the check-to-park lost-wakeup window. Once
/// the sender closes, the reset arm is permanently disarmed so a perpetually
/// ready `changed()` error cannot spin the metadata fetch loop.
struct RestoreEpochSchedule {
    reset_rx: tokio::sync::watch::Receiver<u64>,
    reset_armed: bool,
    poll_interval: Duration,
    fast_interval: Duration,
    fast_until: Option<tokio::time::Instant>,
}

impl RestoreEpochSchedule {
    const FAST_WINDOW: Duration = Duration::from_secs(1);

    fn new(
        transport: crate::bootplan::Transport,
        reset_rx: tokio::sync::watch::Receiver<u64>,
    ) -> Self {
        let poll_interval = match transport {
            crate::bootplan::Transport::Mmds => Duration::from_millis(50),
            crate::bootplan::Transport::Vsock => Duration::from_millis(250),
        };
        // Post-reset fast cadence: MMDS gets are cheap local HTTP; vsock polls
        // reopen the boot-plan connection, so keep them a bit gentler.
        let fast_interval = match transport {
            crate::bootplan::Transport::Mmds => Duration::from_millis(2),
            crate::bootplan::Transport::Vsock => Duration::from_millis(10),
        };
        Self {
            reset_rx,
            reset_armed: true,
            poll_interval,
            fast_interval,
            fast_until: None,
        }
    }

    async fn wait(&mut self) -> RestoreEpochWake {
        let interval = match self.fast_until {
            Some(t) if tokio::time::Instant::now() < t => self.fast_interval,
            _ => {
                self.fast_until = None;
                self.poll_interval
            }
        };

        tokio::select! {
            _ = sleep(interval) => RestoreEpochWake::PollInterval,
            changed = self.reset_rx.changed(), if self.reset_armed => {
                match changed {
                    Ok(()) => {
                        self.fast_until = Some(
                            tokio::time::Instant::now() + Self::FAST_WINDOW
                        );
                        RestoreEpochWake::TransportReset
                    }
                    Err(_) => {
                        self.reset_armed = false;
                        RestoreEpochWake::ResetSenderClosed
                    }
                }
            }
        }
    }

    fn clear_fast_window(&mut self) {
        self.fast_until = None;
    }
}

/// Watch for restore-epoch changes and handle clone restore, over either transport.
///
/// Firecracker polls MMDS; Cloud Hypervisor polls the host's boot-plan vsock port
/// (#632 P2) — both via [`crate::bootplan::fetch_metadata`], so the restore handling is
/// identical. MMDS polls at 50ms (a cheap local HTTP get); vsock polls slower because each
/// poll reopens the boot-plan connection and re-reads the whole plan document.
///
/// The steady poll is only the fallback. The actual restore EVENT is the vsock
/// transport reset the device raises at resume (surfaced by the output writer
/// as `vsock_reset_rx`): a snapshot freezes this loop mid-`sleep`, so with the
/// poll alone a restored clone waits out the frozen sleep's remainder (avg
/// ~25ms of the measured clone floor) before even looking for its epoch. On a
/// reset event the loop fetches immediately and polls at a tight cadence for a
/// bounded window — the host PUTs the epoch right after resume, so this races
/// only a few ms ahead of it. If the event never fires (TTY VMs, non-restore
/// connection errors) behavior degrades to exactly the old poll.
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

    let mut schedule = RestoreEpochSchedule::new(transport, signals.vsock_reset_rx.clone());

    loop {
        if schedule.wait().await == RestoreEpochWake::TransportReset {
            eprintln!("[fc-agent] vsock transport reset observed; fast-polling restore-epoch");
        }

        let boundary_is_armed = crate::snapshot_network::boundary_is_armed();
        let metadata_transport = restore_control.select_transport(boundary_is_armed);
        let metadata = match crate::bootplan::fetch_metadata(metadata_transport).await {
            Ok(value) => value,
            Err(e) => {
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
                    if let Err(error) = signals.restore_status.begin() {
                        eprintln!(
                            "[fc-agent] FATAL: cannot begin clone restore (epoch={}): {:#}; \
                             shutting down clone",
                            current, error
                        );
                        signals.restore_status.fail();
                        crate::system::shutdown_vm(1).await;
                    }
                    // Signal notify_cache_ready_and_wait to stop waiting.
                    // Must be set BEFORE handle_clone_restore so the poll loop
                    // exits before output reconnect changes vsock state.
                    signals
                        .restore_flag
                        .store(true, std::sync::atomic::Ordering::Release);
                    if let Err(error) = crate::restore::handle_clone_restore(
                        &signals,
                        metadata.clone_ipv6.as_deref(),
                        egress_gen_at_last_stable,
                        current,
                        metadata_transport,
                    )
                    .await
                    {
                        signals.restore_status.fail();
                        eprintln!(
                            "[fc-agent] FATAL: clone restore failed closed (epoch={}): {:#}. \
                             Output/exec readiness will not be published; shutting down clone",
                            current, error
                        );
                        crate::system::shutdown_vm(1).await;
                    }
                    // Publish the handled generation in watcher state before
                    // reconnecting output. The host can request another snapshot
                    // as soon as readiness appears; that snapshot must not capture
                    // an old `last_epoch` and mistake its existing listener for a
                    // new restore control generation.
                    last_epoch = Some(current.clone());
                    if let Err(error) = signals.restore_status.succeed() {
                        signals.restore_status.fail();
                        eprintln!(
                            "[fc-agent] FATAL: cannot publish clone restore readiness \
                             (epoch={}): {:#}; shutting down clone",
                            current, error
                        );
                        crate::system::shutdown_vm(1).await;
                    }
                    // Update stable generation after successful restore handling
                    egress_gen_at_last_stable =
                        signals.egress_gen_rx.as_ref().map(|rx| *rx.borrow());
                    // Epoch found and handled — the fast window did its job.
                    schedule.clear_fast_window();
                    // This is the guest's final restore publication edge. It is
                    // intentionally AFTER RestoreStatus::Succeeded and after the
                    // watcher state above is capture-safe. The host gates every
                    // lifecycle/TTY/--exec path on this exact epoch.
                    if let Err(error) = crate::vsock::notify_restore_complete(current).await {
                        signals.restore_status.fail();
                        eprintln!(
                            "[fc-agent] FATAL: restore-completion ACK failed closed \
                             (epoch={} phase=notify-host): {:#}; host lifecycle/exec \
                             readiness will not be published; shutting down clone",
                            current, error
                        );
                        crate::system::shutdown_vm(1).await;
                    }
                }
                Some(prev) if prev != current => {
                    eprintln!("[fc-agent] restore-epoch changed: {} -> {}", prev, current,);
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
                    if let Err(error) = crate::restore::handle_clone_restore(
                        &signals,
                        metadata.clone_ipv6.as_deref(),
                        egress_gen_at_last_stable,
                        current,
                        metadata_transport,
                    )
                    .await
                    {
                        signals.restore_status.fail();
                        eprintln!(
                            "[fc-agent] FATAL: clone restore failed closed (epoch={}): {:#}. \
                             Output/exec readiness will not be published; shutting down clone",
                            current, error
                        );
                        crate::system::shutdown_vm(1).await;
                    }
                    last_epoch = Some(current.clone());
                    if let Err(error) = signals.restore_status.succeed() {
                        signals.restore_status.fail();
                        eprintln!(
                            "[fc-agent] FATAL: cannot publish clone restore readiness \
                             (epoch={}): {:#}; shutting down clone",
                            current, error
                        );
                        crate::system::shutdown_vm(1).await;
                    }
                    // Update stable generation after successful restore handling
                    egress_gen_at_last_stable =
                        signals.egress_gen_rx.as_ref().map(|rx| *rx.borrow());
                    // Epoch found and handled — the fast window did its job.
                    schedule.clear_fast_window();
                    if let Err(error) = crate::vsock::notify_restore_complete(current).await {
                        signals.restore_status.fail();
                        eprintln!(
                            "[fc-agent] FATAL: restore-completion ACK failed closed \
                             (epoch={} phase=notify-host): {:#}; host lifecycle/exec \
                             readiness will not be published; shutting down clone",
                            current, error
                        );
                        crate::system::shutdown_vm(1).await;
                    }
                }
                _ => {}
            }
        }
    }
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

/// Sync VM clock from host time via MMDS.
pub async fn sync_clock_from_host() -> Result<()> {
    eprintln!("[fc-agent] syncing VM clock from host time via MMDS");

    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let token_response = client
        .put("http://169.254.169.254/latest/api/token")
        .header("X-metadata-token-ttl-seconds", "21600")
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .context("getting MMDS token for time sync")?;

    let token = token_response.text().await?;

    let metadata_response = client
        .get("http://169.254.169.254/latest")
        .header("X-metadata-token", &token)
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .context("fetching host-time from MMDS")?;

    let body = metadata_response.text().await?;
    let metadata: LatestMetadata =
        serde_json::from_str(&body).context("parsing host-time from MMDS")?;

    crate::bootplan::set_system_clock(&metadata.host_time).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootplan::Transport;
    use std::future::{poll_fn, Future};
    use std::pin::Pin;
    use std::task::Poll;

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
        let mut schedule = RestoreEpochSchedule::new(crate::bootplan::Transport::Mmds, reset_rx);

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
        let mut schedule = RestoreEpochSchedule::new(crate::bootplan::Transport::Mmds, reset_rx);
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
        let mut schedule = RestoreEpochSchedule::new(crate::bootplan::Transport::Mmds, reset_rx);
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
