use crate::network;
use crate::output::OutputHandle;

/// Handle clone restore: kill stale sockets, flush ARP, reconnect output.
///
/// FUSE volumes are NOT remounted here. The reconnectable multiplexer
/// detects the dead vsock and auto-reconnects to the clone's VolumeServer.
/// The kernel FUSE session stays alive — processes see a brief hang, not errors.
pub async fn handle_clone_restore(output: &OutputHandle) {
    network::kill_stale_tcp_connections().await;
    network::flush_arp_cache().await;
    network::send_gratuitous_arp().await;

    // Reconnect output vsock (broken by snapshot vsock reset).
    // FUSE vsock reconnection is handled automatically by the reconnectable multiplexer.
    output.reconnect();
    eprintln!("[fc-agent] signaled output vsock reconnect after restore");
}
