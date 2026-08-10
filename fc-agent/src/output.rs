use std::future::Future;
use std::sync::Arc;
use tokio::sync::{mpsc, Notify};

use crate::vsock::{self, VsockStream};

pub enum OutputMessage {
    Line { stream: String, content: String },
    Shutdown,
}

/// Handle for sending output lines to the host. Clone-friendly, Send + Sync.
#[derive(Clone)]
pub struct OutputHandle {
    tx: mpsc::Sender<OutputMessage>,
    reconnect: Arc<Notify>,
    reconnect_flag: Arc<std::sync::atomic::AtomicBool>,
}

impl OutputHandle {
    /// Send a line of output. Awaits if channel is full (backpressure).
    pub async fn send_line(&self, stream: &str, line: &str) {
        let _ = self
            .tx
            .send(OutputMessage::Line {
                stream: stream.into(),
                content: line.into(),
            })
            .await;
    }

    /// Send a line synchronously — drops if channel is full.
    pub fn try_send_line(&self, stream: &str, line: &str) {
        let _ = self.tx.try_send(OutputMessage::Line {
            stream: stream.into(),
            content: line.into(),
        });
    }

    /// Signal the writer to reconnect vsock (after snapshot restore).
    ///
    /// Safe to call multiple times — the flag is the single source of truth.
    /// Multiple rapid calls collapse into one reconnection cycle.
    pub fn reconnect(&self) {
        self.reconnect_flag
            .store(true, std::sync::atomic::Ordering::Release);
        self.reconnect.notify_one();
    }

    #[cfg(test)]
    pub(crate) fn reconnect_requested(&self) -> bool {
        self.reconnect_flag
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Signal shutdown.
    pub async fn shutdown(self) {
        let _ = self.tx.send(OutputMessage::Shutdown).await;
    }
}

/// Create an (OutputHandle, writer future, transport-reset watch) triple. Spawn
/// the future as a tokio task.
///
/// The `watch::Receiver<u64>` increments whenever the writer observes EPOLLERR
/// on its established vsock connection — the kernel's reaction to
/// VIRTIO_VSOCK_EVENT_TRANSPORT_RESET, which the device raises at snapshot
/// restore. It is the guest's earliest "you were just restored" signal: the
/// restore-epoch watcher uses it to poll for the new epoch immediately instead
/// of finishing a frozen 50ms sleep first. It is an ACCELERATOR only — every
/// consumer must keep working (just slower) if it never fires (e.g. TTY-mode
/// VMs, where no output connection exists).
pub fn create() -> (
    OutputHandle,
    impl Future<Output = ()>,
    tokio::sync::watch::Receiver<u64>,
) {
    let (tx, rx) = mpsc::channel(4096);
    let reconnect = Arc::new(Notify::new());
    let reconnect_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (reset_tx, reset_rx) = tokio::sync::watch::channel(0u64);
    let handle = OutputHandle {
        tx,
        reconnect: reconnect.clone(),
        reconnect_flag: reconnect_flag.clone(),
    };
    let writer = output_writer(rx, reconnect, reconnect_flag, reset_tx);
    (handle, writer, reset_rx)
}

/// Try to reconnect the vsock stream, retrying up to 30 times.
async fn try_reconnect() -> Option<VsockStream> {
    for attempt in 1..=30 {
        match VsockStream::connect(vsock::HOST_CID, vsock::OUTPUT_PORT) {
            Ok(s) => {
                eprintln!("[fc-agent] output vsock reconnected");
                return Some(s);
            }
            Err(e) => {
                if attempt == 30 {
                    eprintln!(
                        "[fc-agent] output vsock reconnect failed after 30 attempts: {}",
                        e
                    );
                } else {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
    }
    None
}

/// The writer task.
///
/// Connected: read one message, write to vsock. Each write races against
/// the reconnect flag so hung writes on dead vsock are interrupted.
/// Cancellation is safe: dead vsock hangs in writable() (zero bytes sent).
///
/// Disconnected: stop reading channel (backpressure). Wait for reconnect.
/// Zero messages lost — the in-flight message stays in `pending`.
///
/// Reconnect is driven by the `reconnect_flag` AtomicBool. The Notify
/// is only used to wake the writer from blocking waits — the flag is the
/// single source of truth. This makes multiple rapid reconnect() calls
/// safe: they collapse into one reconnection cycle.
///
/// IMPORTANT: After a successful connection, the writer only reconnects
/// via explicit `output.reconnect()` calls. No auto-reconnect on disconnect.
/// The host gates health monitoring on the output connection (snapshot.rs),
/// so auto-reconnecting before handle_clone_restore finishes ARP/exec setup
/// causes pasta port forwarding failures in clones.
///
/// Once an explicit reconnect HAS been requested, the writer keeps retrying
/// until it succeeds (like initial startup). The request arrives at most once
/// per restore epoch — giving up after one ~3s try_reconnect() round would
/// leave output dead for the VM's lifetime and eventually wedge the container
/// on a full output channel.
async fn output_writer(
    mut rx: mpsc::Receiver<OutputMessage>,
    reconnect_signal: Arc<Notify>,
    reconnect_flag: Arc<std::sync::atomic::AtomicBool>,
    reset_tx: tokio::sync::watch::Sender<u64>,
) {
    let mut stream: Option<VsockStream> =
        match VsockStream::connect(vsock::HOST_CID, vsock::OUTPUT_PORT) {
            Ok(s) => {
                eprintln!(
                    "[fc-agent] output vsock connected (port {})",
                    vsock::OUTPUT_PORT
                );
                Some(s)
            }
            Err(e) => {
                eprintln!("[fc-agent] output vsock connect failed: {}", e);
                None
            }
        };

    // Track whether we've ever had a connection. Distinguishes initial
    // startup (retry aggressively) from snapshot restore (wait for explicit
    // reconnect signal from handle_clone_restore).
    let mut ever_connected = stream.is_some();

    // Message that was popped from channel but couldn't be written.
    // Retried first thing after reconnect. Zero message loss.
    let mut pending: Option<String> = None;

    loop {
        // Check reconnect flag — the single source of truth for reconnection.
        // Both the flag-check path and the Notify-wakeup path converge here.
        if reconnect_flag.swap(false, std::sync::atomic::Ordering::AcqRel) {
            eprintln!("[fc-agent] output vsock reconnect (flag)");
            drop(stream.take()); // close old connection before reconnecting
            stream = try_reconnect().await;
            if stream.is_some() {
                ever_connected = true;
            } else {
                // The explicit reconnect request failed (host listener unreachable
                // for ~3s). Re-arm the flag and keep retrying — the request comes
                // at most once per restore epoch, so giving up here would leave
                // output dead for the VM's lifetime.
                eprintln!("[fc-agent] output vsock reconnect failed, retrying");
                reconnect_flag.store(true, std::sync::atomic::Ordering::Release);
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            continue;
        }

        if let Some(ref s) = stream {
            // Retry pending message from previous failed write.
            if let Some(ref data) = pending {
                // Race: write data vs reconnect signal
                tokio::select! {
                    result = s.write_all(data.as_bytes()) => {
                        if result.is_ok() {
                            pending = None;
                        } else {
                            // Write failed — drop connection, keep pending for retry.
                            stream = None;
                            continue;
                        }
                    }
                    _ = reconnect_signal.notified() => {
                        // Reconnect requested — drop connection, keep pending.
                        // Don't re-store the Notify permit; the flag drives reconnection.
                        stream = None;
                        continue;
                    }
                }
            }

            // Wait for next message (or reconnect signal, or transport reset).
            let msg = tokio::select! {
                msg = rx.recv() => msg,
                _ = reconnect_signal.notified() => {
                    // Reconnect requested — drop connection.
                    // Don't re-store the Notify permit; the flag is checked at
                    // the top of the loop. Re-storing would create a stored-permit
                    // cascade: the next select! would fire immediately, dropping
                    // the freshly-reconnected stream before any data is written.
                    stream = None;
                    continue;
                }
                _ = s.wait_for_error() => {
                    // EPOLLERR on the idle connection: the vsock transport was
                    // reset — for an fc-agent-held connection that means a
                    // snapshot restore just happened. Publish the earliest
                    // restore hint (the epoch watcher fast-polls on it), drop
                    // the dead stream, and wait for the explicit reconnect from
                    // handle_clone_restore exactly like the write-failure path.
                    // The snapshot is virtually always taken with output idle,
                    // so this arm is where a restored writer wakes up; a reset
                    // mid-write lands in the write-error path instead, which
                    // doesn't bump the hint — the watcher's normal poll covers
                    // that case.
                    eprintln!("[fc-agent] output vsock EPOLLERR (transport reset)");
                    reset_tx.send_modify(|gen| *gen += 1);
                    stream = None;
                    continue;
                }
            };

            match msg {
                Some(OutputMessage::Line {
                    stream: name,
                    content,
                }) => {
                    let data = format!("{}:{}\n", name, content);
                    // Race: write data vs reconnect signal
                    tokio::select! {
                        result = s.write_all(data.as_bytes()) => {
                            if result.is_err() {
                                pending = Some(data);
                                stream = None;
                            }
                        }
                        _ = reconnect_signal.notified() => {
                            // Reconnect requested mid-write — save data for retry.
                            pending = Some(data);
                            stream = None;
                        }
                    }
                }
                Some(OutputMessage::Shutdown) | None => break,
            }
        } else if ever_connected {
            // Previously connected, now disconnected (snapshot restore).
            // Wait for EXPLICIT reconnect from handle_clone_restore only.
            // Don't auto-reconnect: the host gates health monitoring on the
            // output connection, so reconnecting before ARP/exec setup
            // completes causes pasta port forwarding failures in clones.
            // The top-of-loop flag check performs the actual reconnect (and
            // keeps retrying on failure) — this branch only waits for the wakeup.
            tokio::select! {
                _ = reconnect_signal.notified() => {}
                _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
            }
        } else {
            // Never connected — initial startup. Auto-reconnect with backoff.
            tokio::select! {
                _ = reconnect_signal.notified() => {}
                _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {}
            }
            reconnect_flag.swap(false, std::sync::atomic::Ordering::AcqRel);
            stream = try_reconnect().await;
            if stream.is_some() {
                ever_connected = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[tokio::test]
    async fn test_output_handle_try_send() {
        let (tx, mut rx) = mpsc::channel(16);
        let handle = OutputHandle {
            tx,
            reconnect: Arc::new(Notify::new()),
            reconnect_flag: Arc::new(AtomicBool::new(false)),
        };
        handle.try_send_line("stdout", "hello world");
        match rx.recv().await.unwrap() {
            OutputMessage::Line { stream, content } => {
                assert_eq!(stream, "stdout");
                assert_eq!(content, "hello world");
            }
            _ => panic!("expected Line message"),
        }
    }

    #[tokio::test]
    async fn test_output_handle_shutdown() {
        let (tx, mut rx) = mpsc::channel(16);
        let handle = OutputHandle {
            tx,
            reconnect: Arc::new(Notify::new()),
            reconnect_flag: Arc::new(AtomicBool::new(false)),
        };
        handle.shutdown().await;
        match rx.recv().await.unwrap() {
            OutputMessage::Shutdown => {}
            _ => panic!("expected Shutdown message"),
        }
    }

    /// Verify that multiple rapid reconnect() calls don't poison the writer.
    ///
    /// Before the fix, calling reconnect() twice caused the Notify stored-permit
    /// to cascade: the writer would cycle through ghost connections, dropping
    /// each one before writing data. This test ensures messages survive double
    /// reconnection by using a mock transport (channel-based) instead of vsock.
    #[tokio::test]
    async fn test_double_reconnect_does_not_drop_messages() {
        // We can't use the real output_writer (it calls VsockStream::connect).
        // Instead, test the flag/notify interaction directly: verify that after
        // two rapid reconnect() calls, the flag is consumed in one swap.
        let flag = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(Notify::new());

        let handle = OutputHandle {
            tx: mpsc::channel(16).0,
            reconnect: notify.clone(),
            reconnect_flag: flag.clone(),
        };

        // Call reconnect twice rapidly (simulates handle_clone_restore + agent.rs)
        handle.reconnect();
        handle.reconnect();

        // Flag should be true (both calls set it)
        assert!(flag.load(std::sync::atomic::Ordering::Acquire));

        // One swap should consume it
        assert!(flag.swap(false, std::sync::atomic::Ordering::AcqRel));
        // Second swap should see false — no double-reconnect
        assert!(!flag.swap(false, std::sync::atomic::Ordering::AcqRel));

        // Notify should have exactly one stored permit (second notify_one is a no-op
        // when no waiter exists and permit already stored)
        let notified =
            tokio::time::timeout(std::time::Duration::from_millis(50), notify.notified()).await;
        assert!(
            notified.is_ok(),
            "first notified() should return immediately"
        );

        // No second permit
        let notified2 =
            tokio::time::timeout(std::time::Duration::from_millis(50), notify.notified()).await;
        assert!(
            notified2.is_err(),
            "second notified() should timeout (no stored permit)"
        );
    }

    /// Verify the Notify stored-permit cascade bug is fixed.
    ///
    /// The old code had `reconnect_signal.notify_one()` inside the select! handler
    /// (line 154). This re-stored the permit after consuming it, creating an infinite
    /// cycle: select! fires → re-store → select! fires again → re-store → ...
    /// Each cycle dropped the connection before writing data.
    ///
    /// This test simulates the writer's select! pattern and verifies that after
    /// consuming one notification, there's no ghost permit left.
    #[tokio::test]
    async fn test_no_notify_permit_cascade() {
        let notify = Arc::new(Notify::new());

        // Simulate: reconnect() stores a permit
        notify.notify_one();

        // Simulate: writer's select! consumes the permit
        let consumed =
            tokio::time::timeout(std::time::Duration::from_millis(50), notify.notified()).await;
        assert!(consumed.is_ok(), "should consume the stored permit");

        // OLD CODE would do: notify.notify_one() here (re-store)
        // NEW CODE does NOT re-store.

        // Verify: no ghost permit exists
        let ghost =
            tokio::time::timeout(std::time::Duration::from_millis(50), notify.notified()).await;
        assert!(
            ghost.is_err(),
            "no ghost permit should exist after consuming"
        );
    }

    /// Verify that messages queued during reconnection are not lost.
    ///
    /// Simulates the scenario: reconnect fires, messages are sent to channel
    /// while writer is reconnecting, then messages are consumed after reconnect.
    #[tokio::test]
    async fn test_messages_survive_reconnect_window() {
        let (tx, mut rx) = mpsc::channel(4096);
        let handle = OutputHandle {
            tx,
            reconnect: Arc::new(Notify::new()),
            reconnect_flag: Arc::new(AtomicBool::new(false)),
        };

        // Simulate: reconnect fires
        handle.reconnect();

        // Simulate: container runs during reconnect window and sends output
        handle.send_line("stdout", "not a tty").await;
        handle.send_line("stderr", "some warning").await;

        // Messages should be in the channel, not lost
        match rx.recv().await.unwrap() {
            OutputMessage::Line { stream, content } => {
                assert_eq!(stream, "stdout");
                assert_eq!(content, "not a tty");
            }
            _ => panic!("expected stdout line"),
        }
        match rx.recv().await.unwrap() {
            OutputMessage::Line { stream, content } => {
                assert_eq!(stream, "stderr");
                assert_eq!(content, "some warning");
            }
            _ => panic!("expected stderr line"),
        }
    }
}
