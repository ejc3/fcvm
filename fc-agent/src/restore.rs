use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;

use crate::network;
use crate::output::OutputHandle;

/// Handle clone restore: kill stale sockets, flush ARP, reconnect output + exec.
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

    // Reconnect output vsock (broken by snapshot vsock reset).
    // FUSE vsock reconnection is handled automatically by the reconnectable multiplexer.
    output.reconnect();

    // Re-bind exec server listener (AsyncFd epoll stale after transport reset).
    // Set flag BEFORE notify to prevent race where select! drops the Notified future
    // (see exec.rs doc comment for detailed explanation).
    exec_rebind_needed.store(true, Ordering::Release);
    exec_rebind.notify_one();

    eprintln!("[fc-agent] signaled output + exec vsock reconnect after restore");
}
