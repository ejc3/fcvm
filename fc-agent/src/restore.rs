use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;

use crate::network;
use crate::output::OutputHandle;

/// All signals needed for snapshot restore coordination.
///
/// Groups the exec rebind, egress reconnect, and output reconnect signals
/// that are passed between agent.rs, mmds.rs, and restore.rs.
pub struct RestoreSignals {
    pub output: OutputHandle,
    pub restore_flag: Arc<AtomicBool>,
    pub exec_rebind: Arc<Notify>,
    pub exec_rebind_needed: Arc<AtomicBool>,
    pub exec_rebind_done: Arc<AtomicBool>,
    pub exec_rebind_done_notify: Arc<Notify>,
    pub egress_reconnect: Arc<Notify>,
    pub egress_reconnect_epoch: Arc<AtomicU64>,
    pub egress_reconnect_done: Arc<Notify>,
    pub has_egress_proxy: bool,
}

/// Handle clone restore: kill stale sockets, flush ARP, re-register exec, reconnect output.
///
/// CRITICAL ordering: exec re-register and egress reconnect MUST complete before output reconnect.
/// The host uses the output connection as a readiness signal — once connected,
/// it starts the health monitor which calls `fcvm exec`. If exec's AsyncFd epoll
/// is still stale, health checks hang for ~60s. If egress proxy hasn't reconnected,
/// tests that immediately use egress after health check will fail.
///
/// FUSE volumes are NOT remounted here. The reconnectable multiplexer
/// detects the dead vsock and auto-reconnects to the clone's VolumeServer.
/// The kernel FUSE session stays alive — processes see a brief hang, not errors.
///
/// `clone_ipv6`: For routed mode, the unique per-clone IPv6 that replaces the
/// snapshot's shared guest IPv6 on eth0. Without this, all clones share the same
/// IPv6 and return traffic gets ECMP-routed to the wrong clone.
pub async fn handle_clone_restore(signals: &RestoreSignals, clone_ipv6: Option<&str>) {
    // Reconfigure IPv6 FIRST — before any network traffic can use the old address.
    if let Some(new_ipv6) = clone_ipv6 {
        network::reconfigure_ipv6(new_ipv6).await;
    }

    network::kill_stale_tcp_connections().await;
    network::flush_arp_cache().await;
    network::send_gratuitous_arp().await;

    // FIRST: Re-register exec server listener (AsyncFd epoll stale after transport reset).
    // Reset confirmation flag, then signal. Set flag BEFORE notify to prevent race
    // where select! drops the Notified future (see exec.rs doc comment).
    signals.exec_rebind_done.store(false, Ordering::Release);
    signals.exec_rebind_needed.store(true, Ordering::Release);
    signals.exec_rebind.notify_one();

    // SECOND: Signal egress proxy to reconnect its vsock.
    // Register notified() BEFORE signaling to avoid race: if the proxy already
    // reconnected (reader detected transport reset), done.notify_waiters() fires
    // immediately — without a registered waiter, the permit is lost and we'd
    // time out (5s unnecessary delay). Increment epoch BEFORE notifying so the
    // proxy's two-counter check works: epoch (requested) vs generation (completed).
    let egress_done = if signals.has_egress_proxy {
        let done = signals.egress_reconnect_done.notified();
        signals
            .egress_reconnect_epoch
            .fetch_add(1, Ordering::Release);
        signals.egress_reconnect.notify_waiters();
        Some(done)
    } else {
        None
    };

    // THIRD: Wait for exec server to confirm re-register completed.
    // This ensures accept() works before the host can reach the exec server.
    match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        signals.exec_rebind_done_notify.notified(),
    )
    .await
    {
        Ok(()) => {
            eprintln!("[fc-agent] exec re-registered after restore")
        }
        Err(_) => eprintln!("[fc-agent] WARNING: exec re-register timed out (5s)"),
    }

    // FOURTH: Wait for egress proxy to confirm vsock reconnected.
    // The host gates health monitoring on the output connection — once connected,
    // tests may immediately use egress. We must ensure egress is ready first.
    if let Some(egress_done) = egress_done {
        match tokio::time::timeout(std::time::Duration::from_secs(5), egress_done).await {
            Ok(()) => {
                eprintln!("[fc-agent] egress proxy reconnected after restore")
            }
            Err(_) => eprintln!("[fc-agent] WARNING: egress proxy reconnect timed out (5s)"),
        }
    }

    // FIFTH: Reconnect output vsock (tells host we're alive + exec + egress are ready).
    // FUSE vsock reconnection is handled automatically by the reconnectable multiplexer.
    signals.output.reconnect();

    eprintln!("[fc-agent] restore complete: exec + egress + output reconnected");
}
