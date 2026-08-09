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

    // Track the egress proxy generation at the last stable point.
    // Updated after each handle_clone_restore completes successfully.
    let mut egress_gen_at_last_stable: Option<u64> =
        signals.egress_gen_rx.as_ref().map(|rx| *rx.borrow());

    let mut schedule = RestoreEpochSchedule::new(transport, signals.vsock_reset_rx.clone());

    loop {
        if schedule.wait().await == RestoreEpochWake::TransportReset {
            eprintln!("[fc-agent] vsock transport reset observed; fast-polling restore-epoch");
        }

        let metadata = match crate::bootplan::fetch_metadata(transport).await {
            Ok(m) => m,
            Err(e) => {
                // Log the first few then every 200th failure to avoid spam while still
                // surfacing persistent errors after snapshot restore.
                static FAIL_COUNT: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let count = FAIL_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if count < 5 || count.is_multiple_of(200) {
                    eprintln!(
                        "[fc-agent] restore metadata fetch failed (count={}): {:?}",
                        count, e
                    );
                }
                continue;
            }
        };

        if let Some(ref current) = metadata.restore_epoch {
            match &last_epoch {
                None => {
                    eprintln!("[fc-agent] detected restore-epoch: {}", current,);
                    // Signal notify_cache_ready_and_wait to stop waiting.
                    // Must be set BEFORE handle_clone_restore so the poll loop
                    // exits before output reconnect changes vsock state.
                    signals
                        .restore_flag
                        .store(true, std::sync::atomic::Ordering::Release);
                    crate::restore::handle_clone_restore(
                        &signals,
                        metadata.clone_ipv6.as_deref(),
                        egress_gen_at_last_stable,
                        current,
                        transport,
                    )
                    .await;
                    // Update stable generation after successful restore handling
                    egress_gen_at_last_stable =
                        signals.egress_gen_rx.as_ref().map(|rx| *rx.borrow());
                    last_epoch = metadata.restore_epoch;
                    // Epoch found and handled — the fast window did its job.
                    schedule.clear_fast_window();
                }
                Some(prev) if prev != current => {
                    eprintln!("[fc-agent] restore-epoch changed: {} -> {}", prev, current,);
                    signals
                        .restore_flag
                        .store(true, std::sync::atomic::Ordering::Release);
                    crate::restore::handle_clone_restore(
                        &signals,
                        metadata.clone_ipv6.as_deref(),
                        egress_gen_at_last_stable,
                        current,
                        transport,
                    )
                    .await;
                    // Update stable generation after successful restore handling
                    egress_gen_at_last_stable =
                        signals.egress_gen_rx.as_ref().map(|rx| *rx.borrow());
                    last_epoch = metadata.restore_epoch;
                    // Epoch found and handled — the fast window did its job.
                    schedule.clear_fast_window();
                }
                _ => {}
            }
        }
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
}
