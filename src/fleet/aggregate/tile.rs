//! One tile's reader: subscribe to its renderer stream, decode, hand pictures to the
//! compositor. It reconnects while the tile is away (relaunching), telling the compositor
//! the tile is down so it is drawn black.

use super::codec::{Codec, DecoderIo};
use super::frame::{slot, Frame, Header};
use super::TileEvent;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

pub(crate) async fn read(
    tile: usize,
    url: String,
    codec: Arc<dyn Codec>,
    fps: u32,
    events: mpsc::Sender<TileEvent>,
    cancel: CancellationToken,
) {
    while !cancel.is_cancelled() {
        tokio::select! {
            _ = cancel.cancelled() => return,
            r = stream(tile, &url, codec.as_ref(), fps, &events) => {
                if let Err(e) = r {
                    tracing::debug!(%url, error = %format!("{e:#}"), "tile stream ended");
                }
            }
        }
        if events.send(TileEvent::Down { tile }).await.is_err() {
            return;
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
        }
    }
}

async fn stream(
    tile: usize,
    url: &str,
    codec: &dyn Codec,
    fps: u32,
    events: &mpsc::Sender<TileEvent>,
) -> anyhow::Result<()> {
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await?;
    let mut header: Option<Arc<Header>> = None;
    let mut dec: Option<DecoderIo> = None;
    let mut arrivals: HashMap<i64, (Instant, f64)> = HashMap::new();
    let mut want_key = true;
    loop {
        tokio::select! {
            msg = ws.next() => {
                let Some(msg) = msg else { return Ok(()) };
                match msg? {
                    Message::Text(t) => {
                        let h: Header = serde_json::from_str(&t)?;
                        dec = Some(codec.decoder(h.width, h.height)?);
                        events.send(TileEvent::Up { tile, header: h.clone() }).await.ok();
                        header = Some(Arc::new(h));
                        want_key = true;
                    }
                    Message::Binary(b) => {
                        let Some((tx, _)) = dec.as_ref() else { continue };
                        let f = Frame::parse(&b)?;
                        if want_key && !f.key {
                            continue;
                        }
                        want_key = false;
                        let k = slot(f.t, fps);
                        arrivals.insert(k, (Instant::now(), crate::fleet::now_ms() as f64));
                        if arrivals.len() > 64 {
                            arrivals.retain(|ak, _| *ak > k - 32);
                        }
                        tx.send((f.t, f.au)).await?;
                    }
                    Message::Close(_) => return Ok(()),
                    _ => {}
                }
            }
            out = async { dec.as_mut().unwrap().1.recv().await }, if dec.is_some() => {
                let Some((t, pic)) = out else { anyhow::bail!("the decoder stopped") };
                let k = slot(t, fps);
                let (arrived, arrived_unix_ms) = arrivals.remove(&k).unwrap_or((Instant::now(), crate::fleet::now_ms() as f64));
                let ev = TileEvent::Frame {
                    tile,
                    k,
                    pic: Arc::new(pic),
                    header: header.clone().expect("a decoder implies a header"),
                    arrived,
                    arrived_unix_ms,
                };
                if events.send(ev).await.is_err() {
                    return Ok(());
                }
            }
        }
    }
}
