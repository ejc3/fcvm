//! `fcvm fleet aggregate`: joins a region's tiles into one stream.
//!
//! It subscribes to each tile's renderer stream, decodes every frame, waits until every
//! connected tile has the frame for slot `k` (t = k / 30) or a deadline passes, draws each
//! tile's inner rectangle (crop margins dropped) into the region at that one `t`, and
//! encodes the region once. Viewers read it with the renderer's frame protocol (a text
//! header, then FCVF frames), so a page treats an aggregator as one big tile. A tile that
//! is not streaming (relaunching, dark) is drawn black; the page overlays phases from the
//! shared state.
//!
//! The layout (region, tiles, scene epoch) arrives as `POST /layout` from whoever runs the
//! aggregator: the agent for its host's band, the coordinator for the EC2 half. A layout of
//! a newer generation (a re-tile, taking effect at frame `from_frame`) is swapped in the way
//! the page swaps tiles: the old pipeline keeps composing up to the frame before
//! `from_frame`, while a new pipeline subscribes to the new tiles and waits until every one
//! of them has delivered a frame at or after `from_frame` (or `swap_wait` has passed); then
//! the new pipeline takes over at once, from a frame every tile has. Viewers get a new header
//! and a keyframe at the swap, and never a frame mixing generations. A layout of the same
//! generation (a renderer URL changed) replaces the pipeline at once.

pub mod child;
pub mod codec;
pub mod compose;
pub mod frame;
mod tile;

use crate::cli::{FleetAggregateArgs, FleetAggregateCodec};
use crate::fleet::now_ms;
use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use codec::{Codec, EncodeParams};
use compose::{blit, geometry, tile_height, Picture, BLACK};
use fleet_core::tiling::Rect;
use frame::{slot, Frame, Header};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TileSpec {
    pub id: String,
    pub rect: Rect,
    /// The renderer's frame WebSocket, reachable from the aggregator; `None` when the tile
    /// has nothing to stream (drawn black).
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    pub epoch_unix_ms: u64,
    pub image_aspect: f64,
    pub region: Rect,
    pub tiles: Vec<TileSpec>,
    /// The layout generation, and the frame (k = 30 t) it takes effect at.
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub from_frame: i64,
}

#[derive(Debug, Clone)]
pub struct Options {
    /// The whole image's height in pixels; the region is encoded at its share of it.
    pub image_height: u32,
    pub fps: u32,
    /// Extra query for tile subscriptions (`q=hi`).
    pub tile_query: String,
    /// How long after its slot's wall time a frame waits for late tiles.
    pub late: Duration,
    /// A tile with no frame for the slot shows its newest older one up to this many slots
    /// back, else black.
    pub hold_slots: i64,
    /// How long past a new generation's frame (or its arrival, if later) the old composite
    /// is kept for a new tile that has delivered nothing; then the new one starts without it.
    pub swap_wait: Duration,
    pub encode: EncodeParams,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            image_height: 1080,
            fps: 30,
            tile_query: "q=hi".into(),
            late: Duration::from_millis(150),
            hold_slots: 3,
            swap_wait: Duration::from_secs(2),
            encode: EncodeParams::default(),
        }
    }
}

/// One output message, already serialized.
#[derive(Debug, Clone)]
pub enum Out {
    Header(String),
    Frame { key: bool, bytes: Vec<u8> },
}

#[derive(Default)]
struct Gop {
    header: Option<String>,
    /// From the newest keyframe on.
    frames: Vec<Arc<Out>>,
}

/// Timings of one output frame, milliseconds.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct FrameTiming {
    pub t: f64,
    /// From the last tile frame used to the region frame leaving the encoder.
    pub after_last_tile_ms: f64,
    /// From the first tile frame of the slot to the region frame leaving the encoder.
    pub after_first_tile_ms: f64,
    /// The slowest tile's decode, its access unit in to its picture out.
    pub decode_ms: f64,
    /// The last tile picture decoded to the region composed.
    pub wait_ms: f64,
    pub encode_ms: f64,
    /// Wall clock minus the slot's scene time, for the slowest tile frame and the output.
    pub tile_lateness_ms: f64,
    pub out_lateness_ms: f64,
    pub tiles_exact: usize,
    pub tiles_held: usize,
    pub tiles_black: usize,
    pub bytes: usize,
}

#[derive(Default)]
struct Stats {
    recent: VecDeque<(Instant, FrameTiming)>,
    frames: u64,
    tiles: Vec<serde_json::Value>,
}

pub struct Shared {
    out: broadcast::Sender<Arc<Out>>,
    gop: Mutex<Gop>,
    stats: Mutex<Stats>,
    layout: watch::Sender<Option<Layout>>,
}

impl Shared {
    pub fn new() -> Arc<Shared> {
        Arc::new(Shared {
            out: broadcast::channel(256).0,
            gop: Mutex::default(),
            stats: Mutex::default(),
            layout: watch::channel(None).0,
        })
    }

    pub fn set_layout(&self, l: Layout) {
        self.layout.send_if_modified(|cur| {
            if cur.as_ref() == Some(&l) {
                false
            } else {
                *cur = Some(l);
                true
            }
        });
    }

    fn publish(&self, m: Out) {
        let m = Arc::new(m);
        let mut gop = self.gop.lock().unwrap();
        match &*m {
            Out::Header(h) => {
                gop.header = Some(h.clone());
                gop.frames.clear();
            }
            Out::Frame { key: true, .. } => {
                gop.frames.clear();
                gop.frames.push(m.clone());
            }
            Out::Frame { .. } => {
                if !gop.frames.is_empty() {
                    gop.frames.push(m.clone());
                }
            }
        }
        // Under the lock, so a joining viewer sees each message exactly once: in the
        // snapshot it copies or from its receiver.
        let _ = self.out.send(m);
    }

    /// The header and frames since the last keyframe, and a receiver for what follows.
    fn join(&self) -> (Option<String>, Vec<Arc<Out>>, broadcast::Receiver<Arc<Out>>) {
        let gop = self.gop.lock().unwrap();
        (gop.header.clone(), gop.frames.clone(), self.out.subscribe())
    }

    pub fn stats_json(&self) -> serde_json::Value {
        let s = self.stats.lock().unwrap();
        let mut v: Vec<FrameTiming> = s.recent.iter().map(|(_, f)| *f).collect();
        let pct = |v: &mut Vec<FrameTiming>, f: fn(&FrameTiming) -> f64| {
            let mut xs: Vec<f64> = v.iter().map(f).collect();
            xs.sort_by(f64::total_cmp);
            let at = |q: f64| {
                xs.get(((xs.len() as f64 - 1.0) * q).round() as usize)
                    .copied()
            };
            serde_json::json!({"p50": at(0.5), "p95": at(0.95), "max": xs.last()})
        };
        let span = match (s.recent.front(), s.recent.back()) {
            (Some(a), Some(b)) => b.0.duration_since(a.0).as_secs_f64(),
            _ => 0.0,
        };
        let bytes: usize = v.iter().map(|f| f.bytes).sum();
        serde_json::json!({
            "frames": s.frames,
            "window_frames": v.len(),
            "bitrate_kbps": if span > 0.0 { bytes as f64 * 8.0 / span / 1000.0 } else { 0.0 },
            "after_last_tile_ms": pct(&mut v, |f| f.after_last_tile_ms),
            "after_first_tile_ms": pct(&mut v, |f| f.after_first_tile_ms),
            "decode_ms": pct(&mut v, |f| f.decode_ms),
            "wait_ms": pct(&mut v, |f| f.wait_ms),
            "encode_ms": pct(&mut v, |f| f.encode_ms),
            "tile_lateness_ms": pct(&mut v, |f| f.tile_lateness_ms),
            "out_lateness_ms": pct(&mut v, |f| f.out_lateness_ms),
            "tiles_exact": v.iter().map(|f| f.tiles_exact).sum::<usize>(),
            "tiles_held": v.iter().map(|f| f.tiles_held).sum::<usize>(),
            "tiles_black": v.iter().map(|f| f.tiles_black).sum::<usize>(),
            "tiles": s.tiles,
        })
    }

    fn record(&self, f: FrameTiming) {
        let mut s = self.stats.lock().unwrap();
        s.frames += 1;
        let now = Instant::now();
        s.recent.push_back((now, f));
        while s
            .recent
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) > Duration::from_secs(10))
        {
            s.recent.pop_front();
        }
    }
}

/// A composed slot waiting for the encoder.
struct Pending {
    first_arrival: Instant,
    last_arrival: Instant,
    /// The slowest tile's decode (its access unit in to its picture out).
    decode_ms: f64,
    /// The last picture decoded to the composed region.
    wait_ms: f64,
    composed: Instant,
    tile_late: f64,
    exact: usize,
    held: usize,
    black: usize,
}

/// What a tile reader hands the compositor.
pub(crate) enum TileEvent {
    Up {
        tile: usize,
        header: Header,
    },
    Down {
        tile: usize,
    },
    Frame {
        tile: usize,
        k: i64,
        pic: Arc<Picture>,
        header: Arc<Header>,
        arrived: Instant,
        arrived_unix_ms: f64,
    },
}

struct Held {
    pic: Arc<Picture>,
    header: Arc<Header>,
    arrived: Instant,
    arrived_unix_ms: f64,
    decoded: Instant,
}

/// A running pipeline and the controls the swap uses.
struct Live {
    token: CancellationToken,
    generation: u64,
    /// Whether it publishes (a pipeline of a new generation starts warming, publishing
    /// nothing).
    publish: watch::Sender<bool>,
    /// It composes slots before this one only.
    until: watch::Sender<i64>,
}

fn start(
    shared: &Arc<Shared>,
    codec: &Arc<dyn Codec>,
    opts: &Options,
    layout: Layout,
    publish: bool,
) -> (Live, oneshot::Receiver<()>) {
    let token = CancellationToken::new();
    let (publish_tx, publish_rx) = watch::channel(publish);
    let (until_tx, until_rx) = watch::channel(i64::MAX);
    let (ready_tx, ready_rx) = oneshot::channel();
    let gate = Gate {
        publish: publish_rx,
        until: until_rx,
        ready: Some(ready_tx),
    };
    let live = Live {
        token: token.clone(),
        generation: layout.generation,
        publish: publish_tx,
        until: until_tx,
    };
    let (s, c, o) = (shared.clone(), codec.clone(), opts.clone());
    tokio::spawn(async move {
        if let Err(e) = pipeline(s, c, o, layout, gate, token).await {
            tracing::warn!(error = %format!("{e:#}"), "aggregate pipeline stopped");
        }
    });
    (live, ready_rx)
}

/// Run the aggregator until cancelled: one publishing pipeline per layout, and while a new
/// generation warms up, its pipeline beside the old one.
pub async fn run(
    shared: Arc<Shared>,
    codec: Arc<dyn Codec>,
    opts: Options,
    cancel: CancellationToken,
) {
    let mut layouts = shared.layout.subscribe();
    let mut current: Option<Live> = None;
    let mut warming: Option<(Live, oneshot::Receiver<()>)> = None;
    let mut fresh = true;
    loop {
        if fresh {
            fresh = false;
            if let Some(l) = layouts.borrow_and_update().clone() {
                // A newer layout supersedes one still warming.
                if let Some((w, _)) = warming.take() {
                    w.token.cancel();
                }
                match &current {
                    Some(c) if l.generation > c.generation => {
                        c.until.send_replace(l.from_frame);
                        warming = Some(start(&shared, &codec, &opts, l, false));
                    }
                    _ => {
                        if let Some(c) = current.take() {
                            c.token.cancel();
                        }
                        current = Some(start(&shared, &codec, &opts, l, true).0);
                    }
                }
            }
        }
        let ready = async {
            match warming.as_mut() {
                Some((_, rx)) => rx.await.is_ok(),
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            r = layouts.changed() => {
                if r.is_err() {
                    break;
                }
                fresh = true;
            }
            ok = ready => {
                let (w, _) = warming.take().expect("a warming pipeline");
                if !ok {
                    // It stopped before it was ready; the old one stays.
                    w.token.cancel();
                    continue;
                }
                if let Some(c) = current.take() {
                    c.token.cancel();
                }
                w.publish.send_replace(true);
                current = Some(w);
            }
            _ = cancel.cancelled() => break,
        }
    }
    for l in current.into_iter().chain(warming.map(|(w, _)| w)) {
        l.token.cancel();
    }
}

/// What the swap tells a pipeline, and what it tells the swap.
struct Gate {
    publish: watch::Receiver<bool>,
    until: watch::Receiver<i64>,
    /// Sent once every tile has a frame at or after `from_frame`, or `swap_wait` passed.
    ready: Option<oneshot::Sender<()>>,
}

fn wall_of(epoch_unix_ms: u64, k: i64, fps: u32) -> f64 {
    epoch_unix_ms as f64 + k as f64 * 1000.0 / fps as f64
}

async fn pipeline(
    shared: Arc<Shared>,
    codec: Arc<dyn Codec>,
    opts: Options,
    layout: Layout,
    mut gate: Gate,
    cancel: CancellationToken,
) -> Result<()> {
    let g = geometry(layout.region, layout.image_aspect, opts.image_height);
    let (enc_tx, mut enc_rx) = codec
        .encoder(g.width, g.height, opts.fps, &opts.encode)
        .context("starting the encoder")?;
    let (ev_tx, mut ev_rx) = mpsc::channel::<TileEvent>(256);
    let n = layout.tiles.len();
    for (i, t) in layout.tiles.iter().enumerate() {
        if let Some(url) = &t.url {
            let sep = if url.contains('?') { '&' } else { '?' };
            let mut full = format!("{url}{sep}h={}", tile_height(t.rect, g.full_height));
            if !opts.tile_query.is_empty() {
                full.push('&');
                full.push_str(&opts.tile_query);
            }
            tokio::spawn(tile::read(
                i,
                full,
                codec.clone(),
                opts.fps,
                ev_tx.clone(),
                cancel.clone(),
            ));
        }
    }
    drop(ev_tx);
    let tile_ids: Vec<String> = layout.tiles.iter().map(|t| t.id.clone()).collect();
    let mut header_sent = false;
    let mut frame_no: u32 = 0;
    // Per output slot, what the stats need once the encoder returns it.
    let mut pending: HashMap<i64, Pending> = HashMap::new();

    let mut frames: Vec<BTreeMap<i64, Held>> = (0..n).map(|_| BTreeMap::new()).collect();
    let mut live = vec![false; n];
    let mut next: Option<i64> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(5));
    // A warming pipeline gives up waiting for a silent tile at this wall time.
    let swap_by = (now_ms() as f64).max(wall_of(layout.epoch_unix_ms, layout.from_frame, opts.fps))
        + opts.swap_wait.as_secs_f64() * 1000.0;
    let streams: Vec<usize> = (0..n).filter(|i| layout.tiles[*i].url.is_some()).collect();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            ev = ev_rx.recv() => match ev {
                Some(TileEvent::Up { tile, header }) => {
                    live[tile] = true;
                    tracing::info!(tile = %tile_ids[tile], w = header.width, h = header.height, "tile streaming");
                }
                Some(TileEvent::Down { tile }) => {
                    live[tile] = false;
                    frames[tile].clear();
                }
                Some(TileEvent::Frame { tile, k, pic, header, arrived, arrived_unix_ms }) => {
                    // Frames before the generation's frame belong to the old rectangles.
                    if k >= layout.from_frame {
                        if next.is_none() && *gate.publish.borrow() {
                            next = Some(k);
                        }
                        frames[tile].insert(k, Held { pic, header, arrived, arrived_unix_ms, decoded: Instant::now() });
                    }
                }
                None => {}
            },
            e = enc_rx.recv() => {
                let Some(e) = e else { anyhow::bail!("the encoder stopped") };
                let k = slot(e.t, opts.fps);
                if !header_sent && e.key {
                    let h = serde_json::json!({
                        "codec": frame::codec_string(&e.au).unwrap_or_else(|| codec.name().to_string()),
                        "width": g.width, "height": g.height,
                        "full_width": g.full_width, "full_height": g.full_height,
                        "x0": g.x, "y0": g.y, "fps": opts.fps,
                        "crop": {"left": 0, "top": 0, "right": 0, "bottom": 0},
                        "aggregate": true, "tiles": tile_ids,
                    });
                    shared.publish(Out::Header(h.to_string()));
                    header_sent = true;
                }
                if !header_sent {
                    continue;
                }
                let bytes = Frame {
                    key: e.key,
                    width: g.width as u16,
                    height: g.height as u16,
                    t: e.t,
                    number: frame_no,
                    au: e.au,
                }
                .encode();
                frame_no = frame_no.wrapping_add(1);
                let size = bytes.len();
                shared.publish(Out::Frame { key: e.key, bytes });
                if let Some(p) = pending.remove(&k) {
                    let now = Instant::now();
                    let ms = |a: Instant| now.duration_since(a).as_secs_f64() * 1000.0;
                    shared.record(FrameTiming {
                        t: e.t,
                        after_last_tile_ms: ms(p.last_arrival),
                        after_first_tile_ms: ms(p.first_arrival),
                        decode_ms: p.decode_ms,
                        wait_ms: p.wait_ms,
                        encode_ms: ms(p.composed),
                        tile_lateness_ms: p.tile_late,
                        out_lateness_ms: now_ms() as f64 - wall_of(layout.epoch_unix_ms, k, opts.fps),
                        tiles_exact: p.exact,
                        tiles_held: p.held,
                        tiles_black: p.black,
                        bytes: size,
                    });
                }
                pending.retain(|pk, _| *pk > k);
            }
            _ = tick.tick() => {}
        }
        if !*gate.publish.borrow() {
            // Warming: keep each tile's newest frames, and tell the swap once every tile has
            // one at or after the generation's frame.
            for f in frames.iter_mut() {
                while f.len() as i64 > opts.hold_slots + 1 {
                    f.pop_first();
                }
            }
            let all = streams.iter().all(|i| !frames[*i].is_empty());
            if all || now_ms() as f64 >= swap_by {
                if let Some(r) = gate.ready.take() {
                    let _ = r.send(());
                }
            }
            continue;
        }
        if next.is_none() {
            // Taking over: from the first slot every tile still holds.
            next = frames
                .iter()
                .filter_map(|f| f.keys().next().copied())
                .max()
                .map(|k| k.max(layout.from_frame));
        }
        // With no tile streaming, keep the region going in black on the slot clock.
        if next.is_none() && !live.iter().any(|l| *l) {
            let now_t = (now_ms() as f64 - layout.epoch_unix_ms as f64) / 1000.0;
            next = Some(slot(now_t, opts.fps));
        }
        // Compose every slot that is complete or overdue.
        while let Some(k) = next {
            if k >= *gate.until.borrow() {
                break;
            }
            let due = wall_of(layout.epoch_unix_ms, k, opts.fps) + opts.late.as_secs_f64() * 1000.0;
            let complete =
                (0..n).all(|i| !live[i] || frames[i].keys().next_back().is_some_and(|m| *m >= k));
            let any_live = live.iter().any(|l| *l);
            if !(any_live && complete) && (now_ms() as f64) < due {
                break;
            }
            let mut canvas = Picture::filled(g.width, g.height, BLACK);
            let (mut exact, mut held, mut black) = (0, 0, 0);
            let mut first: Option<Instant> = None;
            let mut last: Option<Instant> = None;
            let mut last_decoded: Option<Instant> = None;
            let mut decode_ms: f64 = 0.0;
            let mut late = f64::MIN;
            for tile_frames in &frames {
                let pick = tile_frames
                    .range(..=k)
                    .next_back()
                    .filter(|(fk, _)| k - **fk <= opts.hold_slots);
                match pick {
                    Some((fk, h)) => {
                        blit(&mut canvas, &g, &h.header, &h.pic);
                        if *fk == k {
                            exact += 1;
                            first = Some(first.map_or(h.arrived, |f| f.min(h.arrived)));
                            last = Some(last.map_or(h.arrived, |l| l.max(h.arrived)));
                            last_decoded =
                                Some(last_decoded.map_or(h.decoded, |l| l.max(h.decoded)));
                            decode_ms = decode_ms
                                .max(h.decoded.duration_since(h.arrived).as_secs_f64() * 1000.0);
                            late = late.max(
                                h.arrived_unix_ms - wall_of(layout.epoch_unix_ms, k, opts.fps),
                            );
                        } else {
                            held += 1;
                        }
                    }
                    None => black += 1,
                }
            }
            let composed = Instant::now();
            pending.insert(
                k,
                Pending {
                    first_arrival: first.unwrap_or(composed),
                    last_arrival: last.unwrap_or(composed),
                    decode_ms,
                    wait_ms: last_decoded
                        .map_or(0.0, |d| composed.duration_since(d).as_secs_f64() * 1000.0),
                    composed,
                    tile_late: if late == f64::MIN { 0.0 } else { late },
                    exact,
                    held,
                    black,
                },
            );
            let t = k as f64 / opts.fps as f64;
            if enc_tx.send((t, canvas)).await.is_err() {
                anyhow::bail!("the encoder stopped");
            }
            for f in frames.iter_mut() {
                let keep = f.split_off(&(k - opts.hold_slots));
                *f = keep;
            }
            // Skip ahead rather than fall further behind the newest tile frames.
            let newest = frames
                .iter()
                .filter_map(|f| f.keys().next_back().copied())
                .max();
            next = Some(match newest {
                Some(m) if m - (k + 1) > 2 * opts.hold_slots => m - opts.hold_slots,
                _ => k + 1,
            });
        }
        let tiles: Vec<serde_json::Value> = (0..n)
            .map(|i| serde_json::json!({"id": tile_ids[i], "streaming": live[i], "newest": frames[i].keys().next_back()}))
            .collect();
        shared.stats.lock().unwrap().tiles = tiles;
    }
}

async fn layout_route(State(app): State<Arc<Shared>>, Json(l): Json<Layout>) -> impl IntoResponse {
    tracing::info!(tiles = l.tiles.len(), region = ?l.region, "layout");
    app.set_layout(l);
    Json(serde_json::json!({"ok": true}))
}

async fn frames_route(ws: WebSocketUpgrade, State(app): State<Arc<Shared>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| viewer(socket, app))
}

async fn viewer(socket: WebSocket, app: Arc<Shared>) {
    let (mut sink, mut stream) = socket.split();
    // Read and ignore what the viewer sends (a height request), so pings get answered.
    let reader = tokio::spawn(async move { while let Some(Ok(_)) = stream.next().await {} });
    let (header, gop, mut rx) = app.join();
    let send = |m: &Out| match m {
        Out::Header(h) => Message::Text(h.clone().into()),
        Out::Frame { bytes, .. } => Message::Binary(bytes.clone().into()),
    };
    let mut ok = true;
    if let Some(h) = header {
        ok = sink.send(Message::Text(h.into())).await.is_ok();
        for m in &gop {
            if !ok {
                break;
            }
            ok = sink.send(send(m)).await.is_ok();
        }
    }
    let mut want_key = false;
    while ok {
        match rx.recv().await {
            Ok(m) => {
                if let Out::Frame { key, .. } = &*m {
                    if want_key && !key {
                        continue;
                    }
                    want_key = false;
                }
                ok = sink.send(send(&m)).await.is_ok();
            }
            // A viewer that fell behind resumes at the next keyframe.
            Err(broadcast::error::RecvError::Lagged(_)) => want_key = true,
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    reader.abort();
}

pub fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route(
            "/layout",
            post(layout_route)
                .get(|State(s): State<Arc<Shared>>| async move { Json(s.layout.borrow().clone()) }),
        )
        .route("/frames", get(frames_route))
        .route(
            "/stats",
            get(|State(s): State<Arc<Shared>>| async move { Json(s.stats_json()) }),
        )
        .route("/healthz", get(|| async { "ok" }))
        .with_state(shared)
}

pub async fn cmd(args: FleetAggregateArgs) -> Result<()> {
    let codec: Arc<dyn Codec> = match args.codec {
        FleetAggregateCodec::Ffmpeg => Arc::new(codec::Ffmpeg { bin: args.ffmpeg }),
        FleetAggregateCodec::Raw => Arc::new(codec::Raw),
    };
    let opts = Options {
        image_height: args.image_height,
        tile_query: args.tile_query,
        late: Duration::from_millis(args.late_ms),
        encode: EncodeParams {
            preset: args.preset,
            crf: args.crf,
            maxrate_kbps: args.maxrate_kbps,
            keyint: 60,
        },
        ..Options::default()
    };
    let shared = Shared::new();
    if let Some(path) = &args.layout {
        let l: Layout = serde_json::from_slice(&std::fs::read(path)?)
            .with_context(|| format!("reading {}", path.display()))?;
        shared.set_layout(l);
    }
    let cancel = CancellationToken::new();
    tokio::spawn(run(shared.clone(), codec, opts, cancel.clone()));
    let listener = tokio::net::TcpListener::bind(&args.listen)
        .await
        .with_context(|| format!("listening on {}", args.listen))?;
    eprintln!("fleet aggregator listening on {}", listener.local_addr()?);
    axum::serve(listener, router(shared)).await?;
    cancel.cancel();
    Ok(())
}
