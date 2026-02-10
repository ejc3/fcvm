use std::future::Future;
use tokio::sync::mpsc;

use crate::vsock::{self, VsockStream};

pub enum OutputMessage {
    Line { stream: String, content: String },
    Reconnect,
    Shutdown,
}

/// Handle for sending output lines to the host. Clone-friendly, Send + Sync.
#[derive(Clone)]
pub struct OutputHandle {
    tx: mpsc::Sender<OutputMessage>,
}

impl OutputHandle {
    /// Send a line of output. Awaits if channel is full.
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
    pub fn reconnect(&self) {
        let _ = self.tx.try_send(OutputMessage::Reconnect);
    }

    /// Signal shutdown — the writer task will exit after draining.
    pub async fn shutdown(self) {
        let _ = self.tx.send(OutputMessage::Shutdown).await;
    }
}

/// Create an (OutputHandle, writer future) pair. Spawn the future as a tokio task.
pub fn create() -> (OutputHandle, impl Future<Output = ()>) {
    let (tx, rx) = mpsc::channel(4096);
    let handle = OutputHandle { tx };
    let writer = output_writer(rx);
    (handle, writer)
}

/// The writer task — receives messages, writes to vsock, handles reconnect.
async fn output_writer(mut rx: mpsc::Receiver<OutputMessage>) {
    let mut stream = match VsockStream::connect(vsock::HOST_CID, vsock::OUTPUT_PORT) {
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

    while let Some(msg) = rx.recv().await {
        match msg {
            OutputMessage::Line { stream: s, content } => {
                if let Some(ref conn) = stream {
                    let data = format!("{}:{}\n", s, content);
                    if let Err(e) = conn.write_all(data.as_bytes()).await {
                        eprintln!("[fc-agent] output write failed: {}", e);
                        stream = None; // Dead — wait for Reconnect
                    }
                }
                // If no connection, line is dropped (transient during snapshot)
            }
            OutputMessage::Reconnect => {
                // Drop old connection (OwnedFd closes automatically), create new
                stream = match VsockStream::connect(vsock::HOST_CID, vsock::OUTPUT_PORT) {
                    Ok(s) => {
                        eprintln!("[fc-agent] output vsock reconnected");
                        Some(s)
                    }
                    Err(e) => {
                        eprintln!("[fc-agent] output vsock reconnect failed: {}", e);
                        None
                    }
                };
            }
            OutputMessage::Shutdown => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_output_handle_try_send() {
        let (tx, mut rx) = mpsc::channel(16);
        let handle = OutputHandle { tx };

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
    async fn test_output_handle_reconnect() {
        let (tx, mut rx) = mpsc::channel(16);
        let handle = OutputHandle { tx };

        handle.reconnect();

        match rx.recv().await.unwrap() {
            OutputMessage::Reconnect => {}
            _ => panic!("expected Reconnect message"),
        }
    }

    #[tokio::test]
    async fn test_output_handle_shutdown() {
        let (tx, mut rx) = mpsc::channel(16);
        let handle = OutputHandle { tx };

        handle.shutdown().await;

        match rx.recv().await.unwrap() {
            OutputMessage::Shutdown => {}
            _ => panic!("expected Shutdown message"),
        }
    }
}
