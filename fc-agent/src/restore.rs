use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;

use crate::network;
use crate::output::OutputHandle;

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
pub async fn handle_clone_restore(
    output: &OutputHandle,
    exec_rebind: &Arc<Notify>,
    exec_rebind_needed: &Arc<AtomicBool>,
    exec_rebind_done: &Arc<AtomicBool>,
    exec_rebind_done_notify: &Arc<Notify>,
    egress_reconnect: &Arc<Notify>,
    egress_reconnect_done: &Arc<Notify>,
    has_egress_proxy: bool,
) {
    network::kill_stale_tcp_connections().await;
    network::flush_arp_cache().await;
    network::send_gratuitous_arp().await;

    // FIRST: Re-register exec server listener (AsyncFd epoll stale after transport reset).
    // Reset confirmation flag, then signal. Set flag BEFORE notify to prevent race
    // where select! drops the Notified future (see exec.rs doc comment).
    exec_rebind_done.store(false, Ordering::Release);
    exec_rebind_needed.store(true, Ordering::Release);
    exec_rebind.notify_one();

    // SECOND: Signal egress proxy to reconnect its vsock.
    // Do this in parallel with exec rebind wait — no reason to serialize.
    if has_egress_proxy {
        egress_reconnect.notify_waiters();
    }

    // THIRD: Wait for exec server to confirm re-register completed.
    // This ensures accept() works before the host can reach the exec server.
    match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        exec_rebind_done_notify.notified(),
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
    if has_egress_proxy {
        match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            egress_reconnect_done.notified(),
        )
        .await
        {
            Ok(()) => {
                eprintln!("[fc-agent] egress proxy reconnected after restore")
            }
            Err(_) => eprintln!("[fc-agent] WARNING: egress proxy reconnect timed out (5s)"),
        }
    }

    // FIFTH: Reconnect output vsock (tells host we're alive + exec + egress are ready).
    // FUSE vsock reconnection is handled automatically by the reconnectable multiplexer.
    output.reconnect();

    eprintln!(
        "[fc-agent] restore complete: exec + egress + output reconnected"
    );
}
