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
    /// Used from non-async contexts (heartbeats, error log capture).
    pub fn try_send_line(&self, stream: &str, line: &str) {
        let _ = self.tx.try_send(OutputMessage::Line {
            stream: stream.into(),
            content: line.into(),
        });
    }

    /// Signal the writer to reconnect vsock (after snapshot restore).
    /// Separate from the message channel so it's never blocked by backpressure.
    pub fn reconnect(&self) {
        self.reconnect.notify_one();
    }

    /// Signal shutdown — the writer task will exit after draining.
    pub async fn shutdown(self) {
        let _ = self.tx.send(OutputMessage::Shutdown).await;
    }
}

/// Create an (OutputHandle, writer future) pair. Spawn the future as a tokio task.
pub fn create() -> (OutputHandle, impl Future<Output = ()>) {
    let (tx, rx) = mpsc::channel(4096);
    let reconnect = Arc::new(Notify::new());
    let handle = OutputHandle {
        tx,
        reconnect: reconnect.clone(),
    };
    let writer = output_writer(rx, reconnect);
    (handle, writer)
}

/// A live vsock connection with death detection.
///
/// Follows fuse-pipe's reconnectable pattern: a reader task on a cloned fd
/// monitors for EOF (from Firecracker's VIRTIO_VSOCK_EVENT_TRANSPORT_RESET
/// during snapshot). The writer races each write against the death signal.
struct OutputConnection {
    stream: VsockStream,
    death: Arc<Notify>,
}

impl OutputConnection {
    /// Connect to the host output vsock and spawn a death-detection reader.
    fn connect() -> Option<Self> {
        let stream = match VsockStream::connect(vsock::HOST_CID, vsock::OUTPUT_PORT) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[fc-agent] output vsock connect failed: {}", e);
                return None;
            }
        };

        let death = Arc::new(Notify::new());

        // Clone the fd and spawn a reader that detects death via EOF.
        // Same pattern as fuse-pipe's reader_loop_reconnectable.
        match stream.try_clone() {
            Ok(reader_fd) => {
                let death_clone = death.clone();
                tokio::spawn(async move {
                    let _ = reader_fd.wait_for_eof().await;
                    death_clone.notify_one();
                });
            }
            Err(e) => {
                eprintln!(
                    "[fc-agent] output vsock clone failed (no death detection): {}",
                    e
                );
            }
        }

        Some(Self { stream, death })
    }

    /// Write data, racing against death notification.
    /// Returns Ok(true) if written, Ok(false) if connection died.
    async fn write(&self, data: &[u8]) -> Result<bool, std::io::Error> {
        tokio::select! {
            result = self.stream.write_all(data) => {
                result?;
                Ok(true)
            }
            _ = self.death.notified() => {
                Ok(false)
            }
        }
    }
}

/// The writer task — receives messages, writes to vsock, handles reconnect.
///
/// When connected: reads one message at a time, writes to vsock. Each write
/// races against death detection (cloned fd EOF, like fuse-pipe).
///
/// When disconnected: stops reading the channel entirely. Backpressure flows
/// naturally — channel fills, send_line blocks, pipe fills, container slows.
/// Only listens for the reconnect signal (separate Notify, never blocked).
/// On reconnect, resumes reading — channel drains, backpressure releases.
///
/// Zero messages dropped. No separate buffer. The mpsc channel IS the buffer.
async fn output_writer(
    mut rx: mpsc::Receiver<OutputMessage>,
    reconnect_signal: Arc<Notify>,
) {
    let mut conn = match OutputConnection::connect() {
        Some(c) => {
            eprintln!(
                "[fc-agent] output vsock connected (port {})",
                vsock::OUTPUT_PORT
            );
            Some(c)
        }
        None => None,
    };

    loop {
        if let Some(ref c) = conn {
            // Connected: process messages, race writes against death
            let msg = tokio::select! {
                msg = rx.recv() => msg,
                _ = c.death.notified() => {
                    eprintln!("[fc-agent] output vsock died (EOF detected)");
                    conn = None;
                    continue;
                }
                _ = reconnect_signal.notified() => {
                    // Snapshot restore — reconnect even if current conn looks alive
                    conn = match OutputConnection::connect() {
                        Some(c) => {
                            eprintln!("[fc-agent] output vsock reconnected");
                            Some(c)
                        }
                        None => {
                            eprintln!("[fc-agent] output vsock reconnect failed");
                            None
                        }
                    };
                    continue;
                }
            };

            match msg {
                Some(OutputMessage::Line { stream, content }) => {
                    let data = format!("{}:{}\n", stream, content);
                    match c.write(data.as_bytes()).await {
                        Ok(true) => {}
                        Ok(false) => {
                            eprintln!("[fc-agent] output vsock died (EOF during write)");
                            conn = None;
                        }
                        Err(e) => {
                            eprintln!("[fc-agent] output write failed: {}", e);
                            conn = None;
                        }
                    }
                }
                Some(OutputMessage::Shutdown) | None => break,
            }
        } else {
            // Disconnected: don't touch the channel. Pure backpressure.
            // Wait only for reconnect signal.
            reconnect_signal.notified().await;
            conn = match OutputConnection::connect() {
                Some(c) => {
                    eprintln!("[fc-agent] output vsock reconnected");
                    Some(c)
                }
                None => {
                    eprintln!("[fc-agent] output vsock reconnect failed");
                    None
                }
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_output_handle_try_send() {
        let (tx, mut rx) = mpsc::channel(16);
        let handle = OutputHandle {
            tx,
            reconnect: Arc::new(Notify::new()),
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
        };

        handle.shutdown().await;

        match rx.recv().await.unwrap() {
            OutputMessage::Shutdown => {}
            _ => panic!("expected Shutdown message"),
        }
    }
}
