//! Newline-delimited JSON over any byte stream: the agent protocol's one framing, shared by
//! the stdio, ssh and (later) WebSocket transports.

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// The longest line accepted. A scene with every bound at its maximum is far below this;
/// a longer line is a broken peer.
pub const MAX_LINE: usize = 1 << 20;

/// Read one message per line until EOF. A line that is not a message is logged and
/// skipped; EOF, a read error or an over-long line ends the stream.
pub struct LineReader<R> {
    inner: BufReader<R>,
    buf: Vec<u8>,
    what: &'static str,
}

impl<R: AsyncRead + Unpin> LineReader<R> {
    pub fn new(inner: R, what: &'static str) -> Self {
        LineReader {
            inner: BufReader::new(inner),
            buf: Vec::new(),
            what,
        }
    }

    /// The next message, or `None` at EOF.
    pub async fn next<T: DeserializeOwned>(&mut self) -> Result<Option<T>> {
        loop {
            self.buf.clear();
            let n = (&mut self.inner)
                .take(MAX_LINE as u64 + 1)
                .read_until(b'\n', &mut self.buf)
                .await
                .with_context(|| format!("reading {}", self.what))?;
            if n == 0 {
                return Ok(None);
            }
            if self.buf.len() > MAX_LINE {
                anyhow::bail!("{} sent a line over {MAX_LINE} bytes", self.what);
            }
            let line = String::from_utf8_lossy(&self.buf);
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str(line) {
                Ok(m) => return Ok(Some(m)),
                Err(e) => {
                    tracing::warn!(peer = self.what, error = %e, line = %line, "skipping a line that is not a message")
                }
            }
        }
    }
}

/// A writer task: messages sent to the returned channel are written one per line and
/// flushed. The task ends when every sender is dropped (shutting the stream down, so the
/// peer reads EOF) or a write fails.
pub fn spawn_writer<T, W>(
    mut out: W,
    what: &'static str,
) -> (mpsc::UnboundedSender<T>, tokio::task::JoinHandle<()>)
where
    T: Serialize + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<T>();
    let task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let mut line = match serde_json::to_vec(&msg) {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!(peer = what, error = %e, "cannot serialize a message");
                    continue;
                }
            };
            line.push(b'\n');
            if let Err(e) = async {
                out.write_all(&line).await?;
                out.flush().await
            }
            .await
            {
                tracing::warn!(peer = what, error = %e, "write failed; closing");
                return;
            }
        }
        // Every sender is gone: tell the peer this side is done.
        let _ = out.shutdown().await;
    });
    (tx, task)
}
