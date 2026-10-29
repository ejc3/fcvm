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

/// `keys`: the layout's admission keys; each attempt subscribes with a token made for it, so a
/// tile that drops after the first token expired is not refused.
pub(crate) async fn read(
    tile: usize,
    base: String,
    keys: Vec<String>,
    codec: Arc<dyn Codec>,
    fps: u32,
    events: mpsc::Sender<TileEvent>,
    cancel: CancellationToken,
) {
    while !cancel.is_cancelled() {
        let url = match super::own_token(&keys) {
            Some(token) => format!("{base}&admit={token}"),
            None => base.clone(),
        };
        tokio::select! {
            _ = cancel.cancelled() => return,
            r = stream(tile, &url, codec.as_ref(), fps, &events) => {
                if let Err(e) = r {
                    tracing::debug!(url = %base, error = %format!("{e:#}"), "tile stream ended");
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
    // The decoder's input and output, held apart so one can be fed while the other drains.
    let mut dec_in: Option<mpsc::Sender<(f64, Vec<u8>)>> = None;
    let mut dec_out: Option<mpsc::Receiver<(f64, super::compose::Picture)>> = None;
    // An access unit read from the tile and not yet taken by the decoder. While it waits
    // nothing more is read from the tile, and the decoder's output is still drained: the
    // decoder can be waiting for room in its output before it takes more input.
    let mut unfed: Option<(f64, Vec<u8>)> = None;
    let mut arrivals: HashMap<i64, (Instant, f64)> = HashMap::new();
    let mut want_key = true;
    loop {
        tokio::select! {
            msg = ws.next(), if unfed.is_none() => {
                let Some(msg) = msg else { return Ok(()) };
                match msg? {
                    Message::Text(t) => {
                        let h: Header = serde_json::from_str(&t)?;
                        let (tx, rx): DecoderIo = codec.decoder(h.width, h.height)?;
                        dec_in = Some(tx);
                        dec_out = Some(rx);
                        events.send(TileEvent::Up { tile, header: h.clone() }).await.ok();
                        header = Some(Arc::new(h));
                        want_key = true;
                    }
                    Message::Binary(b) => {
                        if dec_in.is_none() {
                            continue;
                        }
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
                        unfed = Some((f.t, f.au));
                    }
                    Message::Close(_) => return Ok(()),
                    _ => {}
                }
            }
            // A branch's expression runs even when its condition is false, so the unwrap stays
            // inside the async block.
            room = async { dec_in.clone().expect("a decoder").reserve_owned().await }, if unfed.is_some() => {
                let room = room.map_err(|_| anyhow::anyhow!("the decoder stopped"))?;
                room.send(unfed.take().expect("an access unit"));
            }
            out = async { dec_out.as_mut().expect("a decoder").recv().await }, if dec_out.is_some() => {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::aggregate::codec::Narrow;
    use crate::fleet::aggregate::compose::Picture;
    use axum::extract::ws::{Message as AxMessage, WebSocketUpgrade};
    use axum::routing::get;

    /// A tile that sends its frames faster than they are decoded, into a decoder whose
    /// input and output each hold one message. The reader must keep taking the decoder's
    /// output while it waits to feed it: the decoder takes no more input while its output is
    /// full, so a reader waiting only on the input stops for good. Without that, each frame
    /// the reader feeds while the decoder's output is full has an even chance of being the
    /// one it waits on forever, so 300 frames all get through only by a run of luck far too
    /// long to happen.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reader_drains_the_decoder_while_it_feeds_it() {
        const N: u32 = 300;
        let header = Header {
            codec: "raw".into(),
            width: 2,
            height: 2,
            full_width: 2,
            full_height: 2,
            x0: 0,
            y0: 0,
            fps: 30,
            crop: Default::default(),
        };
        let text = serde_json::to_string(&header).unwrap();
        let app = axum::Router::new().route(
            "/frames",
            get(move |ws: WebSocketUpgrade| {
                let text = text.clone();
                async move {
                    ws.on_upgrade(move |mut s| async move {
                        if s.send(AxMessage::Text(text.into())).await.is_err() {
                            return;
                        }
                        for k in 0..N {
                            let f = Frame {
                                key: true,
                                width: 2,
                                height: 2,
                                t: k as f64 / 30.0,
                                number: k,
                                au: vec![k as u8; Picture::size(2, 2)],
                            };
                            if s.send(AxMessage::Binary(f.encode().into())).await.is_err() {
                                return;
                            }
                        }
                        // Stay connected: the reader ends when the tile does.
                        std::future::pending::<()>().await;
                    })
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/frames", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let (tx, mut rx) = mpsc::channel(256);
        tokio::spawn(async move {
            let codec = Narrow::new(vec![]);
            stream(0, &url, &codec, 30, &tx).await
        });
        let mut frames = 0;
        tokio::time::timeout(Duration::from_secs(10), async {
            while frames < N {
                if let Some(TileEvent::Frame { .. }) = rx.recv().await {
                    frames += 1;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{frames} of {N} frames decoded"));
    }
}
