//! The restore event on Cloud Hypervisor.
//!
//! Firecracker's vsock device raises a transport reset on every restore, which wakes the
//! restore-epoch watcher at once. Cloud Hypervisor's raises none, and the watcher used to
//! learn of a restore from its 250 ms poll of the boot-plan port, so a clone's restore
//! acknowledgement waited out whatever was left of the sleep the snapshot froze (125 to
//! 283 ms, #2191). Instead the host connects to this port right after it resumes the
//! guest. The listener is a guest kernel socket that the snapshot carries, so the connection
//! needs no event from the VMM. Each accepted connection bumps a generation the watcher
//! waits on; the connection carries no data and the watcher reads the restore document
//! from the host's boot-plan listener as it does for a transport reset.

use anyhow::Result;
use tokio::sync::watch;

use crate::vsock::{VsockListener, RESTORE_NUDGE_PORT};

/// Bind the nudge port and serve it. The returned receiver changes once per connection.
/// Binding is synchronous so a failure is reported to the caller, which then keeps the
/// steady poll.
pub fn spawn() -> Result<watch::Receiver<u64>> {
    let listener = VsockListener::bind(RESTORE_NUDGE_PORT)?;
    let (tx, rx) = watch::channel(0u64);
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok(fd) => {
                    drop(fd);
                    tx.send_modify(|generation| *generation += 1);
                }
                Err(e) => {
                    eprintln!("[fc-agent] restore nudge accept error: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
    });
    Ok(rx)
}
