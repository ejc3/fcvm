use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::{watch, Notify};

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
    pub egress_gen_rx: Option<watch::Receiver<u64>>,
    /// NFS mounts from the boot-time plan, kept for post-restore remounting.
    /// MMDS can't be re-fetched here: the host's restore-epoch PUT replaces the
    /// whole MMDS store, so `container-plan` is gone by the time restore runs.
    /// This cache lives in the snapshot's memory image, so a restored VM sees
    /// exactly the mounts that were active when the snapshot was taken.
    pub nfs_mounts: Vec<crate::types::NfsMount>,
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
pub async fn handle_clone_restore(
    signals: &RestoreSignals,
    clone_ipv6: Option<&str>,
    egress_gen_before: Option<u64>,
    restore_epoch: &str,
    transport: crate::bootplan::Transport,
) {
    eprintln!("[fc-agent] handling restore (epoch={})", restore_epoch);

    // Sync clock FIRST — snapshot restore leaves the VM clock frozen at snapshot time.
    // Services that validate timestamps (auth, TLS, sessions) will fail with stale time.
    // Use the active transport: Cloud Hypervisor has no MMDS, so an MMDS fetch would just
    // wait out its timeout and leave the clock stuck at snapshot time.
    if let Err(e) = crate::bootplan::sync_clock_from_host(transport).await {
        eprintln!("[fc-agent] WARNING: clock sync on restore failed: {:?}", e);
    }

    // Reset chrony after clock jump so it doesn't lose its sources.
    // The MMDS sync above stepped the clock, which confuses chrony's
    // offset tracking. `makestep` forces it to accept the new time.
    let _ = tokio::process::Command::new("chronyc")
        .args(["makestep"])
        .output()
        .await;

    // Reconfigure IPv6 — before any network traffic can use the old address.
    if let Some(new_ipv6) = clone_ipv6 {
        network::reconfigure_ipv6(new_ipv6).await;
    }

    network::kill_stale_tcp_connections().await;
    network::flush_arp_cache().await;
    network::send_gratuitous_arp().await;

    // Remount NFS shares: their kernel TCP connections to the host's NFS
    // server died with the snapshot transport reset, and a hard NFS mount
    // wedges every accessor until remounted. Lazy-unmount then mount fresh
    // re-establishes the connection against the host's re-created export.
    // Uses the boot-time plan cached in RestoreSignals — see its doc comment
    // for why MMDS can't be consulted here.
    if !signals.nfs_mounts.is_empty() {
        eprintln!(
            "[fc-agent] remounting {} NFS share(s) after restore",
            signals.nfs_mounts.len()
        );
        for share in &signals.nfs_mounts {
            let _ = tokio::process::Command::new("umount")
                .args(["-l", &share.mount_path])
                .output()
                .await;
        }
        if let Err(e) = crate::mounts::mount_nfs_shares(&signals.nfs_mounts) {
            eprintln!(
                "[fc-agent] WARNING: NFS remount after restore failed: {:?}",
                e
            );
        }
    }

    // FIRST: Re-register exec server listener (AsyncFd epoll stale after transport reset).
    // Reset confirmation flag, then signal. Set flag BEFORE notify to prevent race
    // where select! drops the Notified future (see exec.rs doc comment).
    signals.exec_rebind_done.store(false, Ordering::Release);
    signals.exec_rebind_needed.store(true, Ordering::Release);
    signals.exec_rebind.notify_one();

    // SECOND: Wait for exec server to confirm re-register completed.
    // This ensures accept() works before the host can reach the exec server.
    // Wait on the AtomicBool (the source of truth) and use the Notify only as a
    // wakeup: a stale stored permit (e.g. left over from a previous restore whose
    // wait timed out before the rebind finished) must not let this restore proceed
    // before its own re-register has completed. The flag was reset above, so it only
    // reads true once the exec server has re-registered for THIS restore.
    let rebind_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if signals.exec_rebind_done.load(Ordering::Acquire) {
            eprintln!("[fc-agent] exec re-registered after restore");
            break;
        }
        if tokio::time::Instant::now() >= rebind_deadline {
            eprintln!("[fc-agent] WARNING: exec re-register timed out (5s)");
            break;
        }
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            signals.exec_rebind_done_notify.notified(),
        )
        .await;
    }

    // THIRD: Wait for egress proxy to reconnect (watch channel incremented after vsock connect).
    // No explicit signal needed — the proxy detects the dead vsock fd natively via
    // Interest::ERROR (EPOLLERR fires instantly after transport reset). The proxy's
    // select! arm fires, the session exits, and the reconnect loop connects a new vsock.
    // If proxy already reconnected, wait_for returns immediately (watch retains latest value).
    if let (Some(rx), Some(gen_before)) = (&signals.egress_gen_rx, egress_gen_before) {
        crate::proxy::wait_for_egress_gen(
            rx,
            gen_before,
            std::time::Duration::from_secs(5),
            "reconnected after restore",
        )
        .await;
    }

    // FOURTH: Reconnect output vsock (tells host we're alive + exec + egress are ready).
    // FUSE vsock reconnection is handled automatically by the reconnectable multiplexer.
    signals.output.reconnect();

    // FIFTH: Restart journald. The journal file was mid-write when the snapshot
    // was taken, so the restored journald finds a corrupted file and gets stuck.
    // systemd's watchdog would kill it after 3 min, but we restart it immediately
    // so other services can log via journald right away.
    restart_journald().await;

    eprintln!(
        "[fc-agent] restore complete (epoch={}): exec + egress + output reconnected",
        restore_epoch
    );
}

/// Restart systemd-journald after snapshot restore.
///
/// The journal file is corrupted because journald was mid-write when the snapshot
/// was taken. On restart, journald renames the corrupt file and creates a fresh one.
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
