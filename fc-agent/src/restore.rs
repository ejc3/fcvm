use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;

use crate::network;
use crate::output::OutputHandle;

/// Handle clone restore: kill stale sockets, flush ARP, rebind exec + reconnect output.
///
/// **Ordering invariant**: Exec rebind BEFORE output reconnect. The host uses
/// the output reconnection as a readiness signal — exec must be ready first.
///
/// FUSE volumes are NOT remounted here. The reconnectable multiplexer
/// detects the dead vsock and auto-reconnects to the clone's VolumeServer.
/// The kernel FUSE session stays alive — processes see a brief hang, not errors.
pub async fn handle_clone_restore(
    output: &OutputHandle,
    exec_rebind: &Arc<Notify>,
    exec_rebind_needed: &Arc<AtomicBool>,
) {
    network::kill_stale_tcp_connections().await;
    network::flush_arp_cache().await;
    network::send_gratuitous_arp().await;

    // Re-bind exec server listener FIRST (AsyncFd epoll stale after transport reset).
    // Set flag BEFORE notify to prevent race where select! drops the Notified future
    // (see exec.rs doc comment for detailed explanation).
    exec_rebind_needed.store(true, Ordering::Release);
    exec_rebind.notify_one();

    // Reconnect output vsock AFTER exec rebind. The host uses the output
    // reconnection as a readiness signal, so exec must be ready first.
    // FUSE vsock reconnection is handled automatically by the reconnectable multiplexer.
    output.reconnect();

    eprintln!("[fc-agent] signaled exec rebind + output reconnect after restore");
}
