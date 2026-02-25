use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;

use crate::network;
use crate::output::OutputHandle;

/// Handle clone restore: kill stale sockets, flush ARP, re-register exec, reconnect output.
///
/// CRITICAL ordering: exec re-register MUST complete before output reconnect.
/// The host uses the output connection as a readiness signal — once connected,
/// it starts the health monitor which calls `fcvm exec`. If exec's AsyncFd epoll
/// is still stale, health checks hang for ~60s (see trace evidence in plan).
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

    // SECOND: Wait for exec server to confirm re-register completed.
    // This ensures accept() works before the host can reach the exec server.
    match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        exec_rebind_done_notify.notified(),
    )
    .await
    {
        Ok(()) => {
            eprintln!("[fc-agent] exec re-registered, proceeding to output reconnect")
        }
        Err(_) => eprintln!("[fc-agent] WARNING: exec re-register timed out (5s)"),
    }

    // THIRD: Reconnect output vsock (tells host we're alive + exec is ready).
    // FUSE vsock reconnection is handled automatically by the reconnectable multiplexer.
    output.reconnect();

    // FOURTH: Signal egress proxy to reconnect its vsock (stale after transport reset).
    egress_reconnect.notify_waiters();

    eprintln!(
        "[fc-agent] signaled exec rebind + output reconnect + egress reconnect after restore"
    );
}
