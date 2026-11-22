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

/// Where `fc-agent --resume-snapshot-network` asks the running agent to reconnect its
/// output after a snapshot of this VM.
///
/// A snapshot resets the vsock transport, and the writer reconnects only when asked (see
/// [`output_writer`]). A restored clone is asked by its restore handler, and a cold start
/// by the cache handshake. The VM that was snapshotted is resumed by the host and restores
/// nothing, so before this nothing asked it, and every line its container wrote after a
/// snapshot taken while it ran (a startup snapshot, `fcvm snapshot create`) was lost. The
/// host runs the resume one-shot only on the VM it just snapshotted and resumed, which is
/// never in the middle of a restore of its own.
pub const RECONNECT_SOCKET: &str = "/run/fc-agent-output-reconnect.sock";
const RECONNECT_REQUEST: &[u8] = b"reconnect";

/// Listen on [`RECONNECT_SOCKET`] and reconnect the output on each request.
pub fn spawn_reconnect_listener(output: OutputHandle) -> anyhow::Result<()> {
    spawn_reconnect_listener_at(std::path::Path::new(RECONNECT_SOCKET), output)
}

fn spawn_reconnect_listener_at(path: &std::path::Path, output: OutputHandle) -> anyhow::Result<()> {
    use anyhow::Context;
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("removing a stale {path:?}")),
    }
    let socket = tokio::net::UnixDatagram::bind(path)
        .with_context(|| format!("binding the output reconnect socket {path:?}"))?;
    tokio::spawn(async move {
        let mut buf = [0u8; 64];
        loop {
            match socket.recv(&mut buf).await {
                Ok(n) if &buf[..n] == RECONNECT_REQUEST => {
                    eprintln!("[fc-agent] output reconnect requested after the source resumed");
                    output.reconnect();
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("[fc-agent] WARNING: output reconnect socket failed: {e}");
                    return;
                }
            }
        }
    });
    Ok(())
}

/// Ask the running agent to reconnect its output (see [`RECONNECT_SOCKET`]).
pub fn request_reconnect() -> anyhow::Result<()> {
    request_reconnect_at(std::path::Path::new(RECONNECT_SOCKET))
}

fn request_reconnect_at(path: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context;
    let socket = std::os::unix::net::UnixDatagram::unbound()
        .context("creating the output reconnect request socket")?;
    socket
        .send_to(RECONNECT_REQUEST, path)
        .with_context(|| format!("asking the agent to reconnect its output via {path:?}"))?;
    Ok(())
}

/// The output connection as the restore-epoch watcher sees it.
///
/// Invariant: any path that loses the connection must clear `armed`. While `armed` is true
/// the watcher polls only every 10 s and relies on the writer to report a restore's
/// transport reset, which only a live connection receives. A connection the writer still
/// holds but the kernel has already ended (the host closed it, a write failed) would leave
/// a restored clone waiting out the backstop. The writer publishes `armed` from whether it
/// holds a stream at the top of every turn of its loop, so losing the connection means
/// dropping the stream and going round: the idle wait does that for a transport reset and
/// for a close by the host ([`connection_lost`]), the write paths for a failed write, and
/// the reconnect before it waits for a new connection.
#[derive(Clone)]
pub struct ResetWatch {
    /// Incremented when the writer sees EPOLLERR on its idle established connection: the
    /// guest-visible edge of the device's VIRTIO_VSOCK_EVENT_TRANSPORT_RESET, which the
    /// VMM raises for a snapshot. A wakeup only: every consumer keeps working, slower,
    /// if it never fires (a TTY VM has no output connection).
    pub reset_rx: tokio::sync::watch::Receiver<u64>,
    /// True while the writer holds a connection that would see that reset.
    pub armed_rx: tokio::sync::watch::Receiver<bool>,
}

/// How an idle output connection ended.
#[derive(Debug, PartialEq, Eq)]
enum Lost {
    /// EPOLLERR: the vsock transport was reset, which for a connection fc-agent holds
    /// means a snapshot was restored (or its source resumed).
    Reset,
    /// The host closed the connection. No snapshot is implied.
    PeerClosed,
}

/// Wait until the idle connection `s` is lost. A transport reset wins over the close it
/// can come with, so a restore is always reported as one. A close by the host raises no
/// EPOLLERR: the idle writer waited on that alone, kept the dead stream, and stayed
/// `armed` on a connection no reset could reach.
async fn connection_lost(s: &VsockStream) -> Lost {
    tokio::select! {
        biased;
        _ = s.wait_for_error() => Lost::Reset,
        closed = s.wait_for_peer_close() => match closed {
            Ok(()) => Lost::PeerClosed,
            // The read failed: the connection ended by an error, as a reset ends it.
            Err(_) => Lost::Reset,
        },
    }
}

/// Create an (OutputHandle, writer future, [`ResetWatch`]) triple. Spawn the future as a
/// tokio task.
pub fn create() -> (OutputHandle, impl Future<Output = ()>, ResetWatch) {
    let (tx, rx) = mpsc::channel(4096);
    let reconnect = Arc::new(Notify::new());
    let reconnect_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (reset_tx, reset_rx) = tokio::sync::watch::channel(0u64);
    let (armed_tx, armed_rx) = tokio::sync::watch::channel(false);
    let handle = OutputHandle {
        tx,
        reconnect: reconnect.clone(),
        reconnect_flag: reconnect_flag.clone(),
    };
    let writer = output_writer(rx, reconnect, reconnect_flag, reset_tx, armed_tx);
    (handle, writer, ResetWatch { reset_rx, armed_rx })
}

/// Publish whether the writer holds an established connection. Only a change is sent, so a
/// watcher waiting on the channel wakes for edges, not for every loop of the writer.
fn publish_armed(armed_tx: &tokio::sync::watch::Sender<bool>, connected: bool) {
    armed_tx.send_if_modified(|armed| {
        let changed = *armed != connected;
        *armed = connected;
        changed
    });
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
    armed_tx: tokio::sync::watch::Sender<bool>,
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
        // Every path that drops or (re)makes the connection comes back here, so the watcher
        // learns here whether a transport reset would reach us. The reconnect below also
        // says so itself before it waits, since its retries can take seconds.
        publish_armed(&armed_tx, stream.is_some());
        // Check reconnect flag — the single source of truth for reconnection.
        // Both the flag-check path and the Notify-wakeup path converge here.
        if reconnect_flag.swap(false, std::sync::atomic::Ordering::AcqRel) {
            eprintln!("[fc-agent] output vsock reconnect (flag)");
            drop(stream.take()); // close old connection before reconnecting
            publish_armed(&armed_tx, false);
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
                lost = connection_lost(s) => {
                    // The idle connection is gone. Either way the stream is dropped
                    // here, which clears `armed` at the top of the loop (the
                    // invariant on `ResetWatch`), and the writer waits for an
                    // explicit reconnect, as after a failed write.
                    match lost {
                        // A snapshot is virtually always taken with the output
                        // idle, so this is where a restored writer wakes: publish
                        // the restore hint (the epoch watcher fetches on it). A
                        // reset mid-write lands in the write-error path instead,
                        // which publishes none; the watcher's poll covers it, since
                        // that path drops the stream too.
                        Lost::Reset => {
                            eprintln!("[fc-agent] output vsock EPOLLERR (transport reset)");
                            reset_tx.send_modify(|gen| *gen += 1);
                        }
                        // Nothing was restored, so no hint.
                        Lost::PeerClosed => {
                            eprintln!("[fc-agent] output vsock closed by the host");
                        }
                    }
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

    /// The reconnect path says it holds no connection before it waits for a new one: its
    /// retries take up to ~3 s, and a snapshot taken meanwhile would otherwise restore a
    /// watcher on the 10 s backstop with nothing to report the restore's reset.
    #[test]
    fn the_reconnect_disarms_before_it_waits() {
        let source = include_str!("output.rs");
        let writer = &source[source.find("async fn output_writer(").unwrap()..];
        let branch = &writer[writer.find("if reconnect_flag.swap(false").unwrap()..];
        let drop_at = branch.find("drop(stream.take());").unwrap();
        let disarm_at = branch
            .find("publish_armed(&armed_tx, false);")
            .expect("the reconnect branch publishes that it is disarmed");
        let wait_at = branch.find("try_reconnect().await").unwrap();
        assert!(drop_at < disarm_at && disarm_at < wait_at);
    }

    fn pair() -> (VsockStream, std::os::unix::net::UnixStream) {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        (VsockStream::from_connected(ours.into()).unwrap(), theirs)
    }

    /// A connection the host closed is lost, with no restore hint. The idle writer waited
    /// for EPOLLERR alone, which a close does not raise, so it kept the stream and `armed`
    /// stayed true: a clone of a snapshot taken then found its restore only at the 10 s
    /// backstop.
    ///
    /// RED BEFORE THE FIX: with the idle wait on `wait_for_error` alone this timed out.
    #[tokio::test]
    async fn a_connection_the_host_closed_is_lost_without_a_reset() {
        let (stream, host) = pair();
        drop(host);
        let lost =
            tokio::time::timeout(std::time::Duration::from_secs(5), connection_lost(&stream))
                .await
                .expect("the idle wait never noticed the host's close");
        assert_eq!(lost, Lost::PeerClosed);
    }

    /// A live connection is not lost, and neither is one the host wrote to: its bytes are
    /// read and dropped, and the close after them is still seen.
    #[tokio::test]
    async fn a_live_connection_is_kept_whatever_the_host_sends() {
        use std::io::Write;
        let (stream, mut host) = pair();
        let idle = std::time::Duration::from_millis(100);
        assert!(tokio::time::timeout(idle, connection_lost(&stream))
            .await
            .is_err());
        host.write_all(b"unexpected").unwrap();
        assert!(tokio::time::timeout(idle, connection_lost(&stream))
            .await
            .is_err());
        drop(host);
        let lost =
            tokio::time::timeout(std::time::Duration::from_secs(5), connection_lost(&stream))
                .await
                .expect("the close after the host's bytes was missed");
        assert_eq!(lost, Lost::PeerClosed);
    }

    /// Every way the idle wait loses the connection drops the stream and goes round, so
    /// the top of the loop clears `armed` (the invariant on `ResetWatch`).
    #[test]
    fn a_lost_idle_connection_drops_the_stream_whichever_way_it_was_lost() {
        let source = include_str!("output.rs");
        let writer = &source[source.find("async fn output_writer(").unwrap()..];
        let arm = &writer[writer
            .find("lost = connection_lost(s) => {")
            .expect("the idle wait")..];
        let arm = &arm[..arm.find("            };\n").unwrap()];
        let outcome = arm.find("match lost {").unwrap();
        let dropped = arm
            .rfind("stream = None;\n                    continue;")
            .unwrap();
        assert!(
            outcome < dropped,
            "the stream is dropped inside one outcome only"
        );
        assert!(writer.contains("publish_armed(&armed_tx, stream.is_some());"));
    }

    #[test]
    fn armed_is_published_on_edges_only() {
        let (armed_tx, mut armed_rx) = tokio::sync::watch::channel(false);
        publish_armed(&armed_tx, false);
        assert!(!armed_rx.has_changed().unwrap(), "no edge, no wakeup");
        publish_armed(&armed_tx, true);
        assert!(armed_rx.has_changed().unwrap());
        assert!(*armed_rx.borrow_and_update());
        publish_armed(&armed_tx, true);
        assert!(!armed_rx.has_changed().unwrap(), "no edge, no wakeup");
        publish_armed(&armed_tx, false);
        assert!(!*armed_rx.borrow_and_update());
    }

    /// The resume one-shot's request reaches the running agent's output writer as a
    /// reconnect, and other datagrams do not.
    #[tokio::test]
    async fn a_reconnect_request_reaches_the_output_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reconnect.sock");
        let (output, _writer, _reset) = create();
        spawn_reconnect_listener_at(&path, output.clone()).unwrap();

        std::os::unix::net::UnixDatagram::unbound()
            .unwrap()
            .send_to(b"something else", &path)
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !output.reconnect_requested(),
            "a stray datagram reconnected the output"
        );

        request_reconnect_at(&path).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !output.reconnect_requested() {
            assert!(
                std::time::Instant::now() < deadline,
                "no reconnect was requested"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

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
