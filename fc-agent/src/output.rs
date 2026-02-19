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
    pub fn reconnect(&self) {
        self.reconnect_flag
            .store(true, std::sync::atomic::Ordering::Release);
        self.reconnect.notify_one();
    }

    /// Signal shutdown.
    pub async fn shutdown(self) {
        let _ = self.tx.send(OutputMessage::Shutdown).await;
    }
}

/// Create an (OutputHandle, writer future) pair. Spawn the future as a tokio task.
pub fn create() -> (OutputHandle, impl Future<Output = ()>) {
    let (tx, rx) = mpsc::channel(4096);
    let reconnect = Arc::new(Notify::new());
    let reconnect_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handle = OutputHandle {
        tx,
        reconnect: reconnect.clone(),
        reconnect_flag: reconnect_flag.clone(),
    };
    let writer = output_writer(rx, reconnect, reconnect_flag);
    (handle, writer)
}

/// Try to write, racing against the reconnect signal.
/// Returns true if written. Returns false if reconnect fired (write cancelled,
/// no bytes sent — safe because dead vsock hangs in writable() before writing).
async fn write_or_reconnect(stream: &VsockStream, data: &[u8], reconnect: &Arc<Notify>) -> bool {
    tokio::select! {
        result = stream.write_all(data) => result.is_ok(),
        _ = reconnect.notified() => {
            reconnect.notify_one(); // re-store for disconnected mode
            false
        }
    }
}

/// The writer task.
///
/// Connected: read one message, write to vsock. Each write races against
/// the reconnect signal so hung writes on dead vsock are interrupted.
/// Cancellation is safe: dead vsock hangs in writable() (zero bytes sent).
///
/// Disconnected: stop reading channel (backpressure). Wait for reconnect.
/// Zero messages lost — the in-flight message stays in `pending`.
async fn output_writer(
    mut rx: mpsc::Receiver<OutputMessage>,
    reconnect_signal: Arc<Notify>,
    reconnect_flag: Arc<std::sync::atomic::AtomicBool>,
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

    // Message that was popped from channel but couldn't be written.
    // Retried first thing after reconnect. Zero message loss.
    let mut pending: Option<String> = None;

    loop {
        // Check reconnect flag — catches signals lost by Notify drop in select!
        if reconnect_flag.swap(false, std::sync::atomic::Ordering::AcqRel) {
            eprintln!("[fc-agent] output vsock reconnect (flag)");
            stream = None;
            // Reconnect immediately — don't wait for Notify
            for attempt in 1..=30 {
                match VsockStream::connect(vsock::HOST_CID, vsock::OUTPUT_PORT) {
                    Ok(s) => {
                        eprintln!("[fc-agent] output vsock reconnected");
                        stream = Some(s);
                        break;
                    }
                    Err(e) => {
                        if attempt == 30 {
                            eprintln!("[fc-agent] output vsock reconnect failed: {}", e);
                        } else {
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                    }
                }
            }
            continue;
        }

        if let Some(ref s) = stream {
            // Retry pending message from previous failed write.
            if let Some(ref data) = pending {
                if write_or_reconnect(s, data.as_bytes(), &reconnect_signal).await {
                    pending = None;
                } else {
                    // Reconnect signal fired — drop connection, keep pending.
                    stream = None;
                    continue;
                }
            }

            // Wait for next message (or reconnect signal).
            let msg = tokio::select! {
                msg = rx.recv() => msg,
                _ = reconnect_signal.notified() => {
                    stream = None;
                    reconnect_signal.notify_one();
                    continue;
                }
            };

            match msg {
                Some(OutputMessage::Line {
                    stream: name,
                    content,
                }) => {
                    let data = format!("{}:{}\n", name, content);
                    if !write_or_reconnect(s, data.as_bytes(), &reconnect_signal).await {
                        pending = Some(data);
                        stream = None;
                    }
                }
                Some(OutputMessage::Shutdown) | None => break,
            }
        } else {
            // Disconnected: backpressure. Wait for reconnect signal, then retry connect.
            reconnect_signal.notified().await;
            for attempt in 1..=30 {
                match VsockStream::connect(vsock::HOST_CID, vsock::OUTPUT_PORT) {
                    Ok(s) => {
                        eprintln!("[fc-agent] output vsock reconnected");
                        stream = Some(s);
                        break;
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
            reconnect_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            reconnect_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        handle.shutdown().await;
        match rx.recv().await.unwrap() {
            OutputMessage::Shutdown => {}
            _ => panic!("expected Shutdown message"),
        }
    }
}
