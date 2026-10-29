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
pub mod push;
mod tile;
pub mod tls;

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
    /// The coordinator's admission keys (hex): with any, the public listener's `/frames`
    /// needs a direct token made with one, and this aggregator's own tile subscriptions carry
    /// one it makes with the first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub admit_keys: Vec<String>,
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
    /// Rounds of the compositor's loop (tests: a loop that never sleeps shows here).
    rounds: u64,
}

pub struct Shared {
    out: broadcast::Sender<Arc<Out>>,
    gop: Mutex<Gop>,
    stats: Mutex<Stats>,
    layout: watch::Sender<Option<Layout>>,
    /// The bearer token `POST /layout` needs; without one only this host may post.
    layout_token: std::sync::OnceLock<String>,
}

impl Shared {
    pub fn new() -> Arc<Shared> {
        Arc::new(Shared {
            out: broadcast::channel(256).0,
            gop: Mutex::default(),
            stats: Mutex::default(),
            layout: watch::channel(None).0,
            layout_token: std::sync::OnceLock::new(),
        })
    }

    /// Require `token` on every `POST /layout` (set once, before serving).
    pub fn require_layout_token(&self, token: String) {
        let _ = self.layout_token.set(token);
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

/// Frames each tile of a warming pipeline keeps (half a second): the new composite starts at
/// the first frame every tile still holds, so tiles up to this far apart in time still let
/// it start at the generation's frame.
const WARM_FRAMES: usize = 15;

/// The first wait before a publishing pipeline that stopped is started again; it doubles
/// up to [`RESTART_MAX`], and starts over once a pipeline has run [`RESTART_RESET`].
const RESTART_MIN: Duration = Duration::from_secs(1);
const RESTART_MAX: Duration = Duration::from_secs(30);
const RESTART_RESET: Duration = Duration::from_secs(60);

/// A running pipeline and the controls the swap uses.
struct Live {
    token: CancellationToken,
    generation: u64,
    /// Whether it publishes (a pipeline of a new generation starts warming, publishing
    /// nothing).
    publish: watch::Sender<bool>,
    /// It composes slots before this one only.
    until: watch::Sender<i64>,
    /// The layout it composes, to start it again with.
    layout: Layout,
    started: Instant,
    /// Resolves when the pipeline's task has ended, for any reason.
    exited: oneshot::Receiver<()>,
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
    let (exit_tx, exit_rx) = oneshot::channel();
    let live = Live {
        token: token.clone(),
        generation: layout.generation,
        publish: publish_tx,
        until: until_tx,
        layout: layout.clone(),
        started: Instant::now(),
        exited: exit_rx,
    };
    let (s, c, o) = (shared.clone(), codec.clone(), opts.clone());
    tokio::spawn(async move {
        if let Err(e) = pipeline(s, c, o, layout, gate, token).await {
            tracing::warn!(error = %format!("{e:#}"), "aggregate pipeline stopped");
        }
        // Dropped unsent if the pipeline panicked, which the receiver sees the same way.
        let _ = exit_tx.send(());
    });
    (live, ready_rx)
}

/// Run the aggregator until cancelled: one publishing pipeline per layout, and while a new
/// generation warms up, its pipeline beside the old one. A publishing pipeline that stops
/// (its encoder failed) is started again after a backoff.
pub async fn run(
    shared: Arc<Shared>,
    codec: Arc<dyn Codec>,
    opts: Options,
    cancel: CancellationToken,
) {
    let mut layouts = shared.layout.subscribe();
    let mut current: Option<Live> = None;
    let mut warming: Option<(Live, oneshot::Receiver<()>)> = None;
    // The layout of a publishing pipeline that stopped, and when to start it again.
    let mut restart: Option<(Layout, tokio::time::Instant)> = None;
    let mut backoff = RESTART_MIN;
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
                        restart = None;
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
        let exited = async {
            match current.as_mut() {
                Some(c) => {
                    let _ = (&mut c.exited).await;
                }
                None => std::future::pending().await,
            }
        };
        let restart_due = async {
            match &restart {
                Some((_, at)) => tokio::time::sleep_until(*at).await,
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
                    // It stopped before it was ready; the old one stays, and composes past
                    // the frame it was to stop at.
                    w.token.cancel();
                    if let Some(c) = &current {
                        c.until.send_replace(i64::MAX);
                    }
                    continue;
                }
                if let Some(c) = current.take() {
                    c.token.cancel();
                }
                restart = None;
                w.publish.send_replace(true);
                current = Some(w);
            }
            () = exited => {
                let c = current.take().expect("a publishing pipeline");
                // Its tile readers hold clones of the token and outlive the pipeline's task.
                c.token.cancel();
                if c.started.elapsed() >= RESTART_RESET {
                    backoff = RESTART_MIN;
                }
                tracing::warn!(generation = c.generation, retry_in = ?backoff, "the publishing pipeline stopped; starting it again");
                restart = Some((c.layout, tokio::time::Instant::now() + backoff));
                backoff = (backoff * 2).min(RESTART_MAX);
            }
            () = restart_due => {
                let (l, _) = restart.take().expect("a pipeline to start again");
                let live = start(&shared, &codec, &opts, l, true).0;
                // A newer generation still warming takes over at its frame.
                if let Some((w, _)) = &warming {
                    live.until.send_replace(w.layout.from_frame);
                }
                current = Some(live);
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

/// What a pipeline does with the encoder's output: the header before the first keyframe,
/// every frame to the viewers, and each slot's timings.
struct Emitter {
    shared: Arc<Shared>,
    g: compose::Geometry,
    fps: u32,
    epoch_unix_ms: u64,
    codec_name: &'static str,
    tile_ids: Vec<String>,
    header_sent: bool,
    frame_no: u32,
    /// Per output slot, what the stats need once the encoder returns it.
    pending: HashMap<i64, Pending>,
}

impl Emitter {
    fn emit(&mut self, e: codec::Encoded) {
        let g = self.g;
        let k = slot(e.t, self.fps);
        if !self.header_sent && e.key {
            let h = serde_json::json!({
                "codec": frame::codec_string(&e.au).unwrap_or_else(|| self.codec_name.to_string()),
                "width": g.width, "height": g.height,
                "full_width": g.full_width, "full_height": g.full_height,
                "x0": g.x, "y0": g.y, "fps": self.fps,
                "crop": {"left": 0, "top": 0, "right": 0, "bottom": 0},
                "aggregate": true, "tiles": self.tile_ids,
            });
            self.shared.publish(Out::Header(h.to_string()));
            self.header_sent = true;
        }
        if !self.header_sent {
            return;
        }
        let bytes = Frame {
            key: e.key,
            width: g.width as u16,
            height: g.height as u16,
            t: e.t,
            number: self.frame_no,
            au: e.au,
        }
        .encode();
        self.frame_no = self.frame_no.wrapping_add(1);
        let size = bytes.len();
        self.shared.publish(Out::Frame { key: e.key, bytes });
        if let Some(p) = self.pending.remove(&k) {
            let now = Instant::now();
            let ms = |a: Instant| now.duration_since(a).as_secs_f64() * 1000.0;
            self.shared.record(FrameTiming {
                t: e.t,
                after_last_tile_ms: ms(p.last_arrival),
                after_first_tile_ms: ms(p.first_arrival),
                decode_ms: p.decode_ms,
                wait_ms: p.wait_ms,
                encode_ms: ms(p.composed),
                tile_lateness_ms: p.tile_late,
                out_lateness_ms: now_ms() as f64 - wall_of(self.epoch_unix_ms, k, self.fps),
                tiles_exact: p.exact,
                tiles_held: p.held,
                tiles_black: p.black,
                bytes: size,
            });
        }
        self.pending.retain(|pk, _| *pk > k);
    }
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
                layout.admit_keys.clone(),
                codec.clone(),
                opts.fps,
                ev_tx.clone(),
                cancel.clone(),
            ));
        }
    }
    drop(ev_tx);
    let tile_ids: Vec<String> = layout.tiles.iter().map(|t| t.id.clone()).collect();
    let mut out = Emitter {
        shared: shared.clone(),
        g,
        fps: opts.fps,
        epoch_unix_ms: layout.epoch_unix_ms,
        codec_name: codec.name(),
        tile_ids: tile_ids.clone(),
        header_sent: false,
        frame_no: 0,
        pending: HashMap::new(),
    };

    let mut frames: Vec<BTreeMap<i64, Held>> = (0..n).map(|_| BTreeMap::new()).collect();
    let mut live = vec![false; n];
    let mut next: Option<i64> = None;
    // Until this slot the compositor does not skip ahead (the backlog of a takeover).
    let mut catch_up_until = i64::MIN;
    let mut tick = tokio::time::interval(Duration::from_millis(5));
    // A warming pipeline gives up waiting for a silent tile at this wall time.
    let swap_by = (now_ms() as f64).max(wall_of(layout.epoch_unix_ms, layout.from_frame, opts.fps))
        + opts.swap_wait.as_secs_f64() * 1000.0;
    let streams: Vec<usize> = (0..n).filter(|i| layout.tiles[*i].url.is_some()).collect();
    // With no tile streaming no reader holds a sender, and a closed channel is always
    // ready: stop polling it, and let the timers pace the black region.
    let mut events_open = true;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            ev = ev_rx.recv(), if events_open => match ev {
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
                        frames[tile].insert(k, Held { pic, header, arrived, arrived_unix_ms, decoded: Instant::now() });
                    }
                }
                None => events_open = false,
            },
            e = enc_rx.recv() => {
                let Some(e) = e else { anyhow::bail!("the encoder stopped") };
                out.emit(e);
            }
            _ = tick.tick() => {}
        }
        if !*gate.publish.borrow() {
            // Warming: keep each tile's frames from the generation's frame on (at most
            // WARM_FRAMES, so a slow tile does not make the others skip ahead), and tell the
            // swap once every tile has one.
            for f in frames.iter_mut() {
                while f.len() > WARM_FRAMES {
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
            // Taking over: from the first slot every tile still holds, composing what was
            // kept while warming without skipping ahead over it.
            next = frames
                .iter()
                .filter_map(|f| f.keys().next().copied())
                .max()
                .map(|k| k.max(layout.from_frame));
            if let Some(k) = next {
                catch_up_until = k + WARM_FRAMES as i64;
            }
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
            out.pending.insert(
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
            // Keep taking the encoder's output while waiting for room in its input: a
            // backlog of overdue slots would otherwise fill both and wait on itself.
            let room = loop {
                tokio::select! {
                    room = enc_tx.reserve() => break room,
                    e = enc_rx.recv() => {
                        let Some(e) = e else { anyhow::bail!("the encoder stopped") };
                        out.emit(e);
                    }
                    _ = cancel.cancelled() => return Ok(()),
                }
            };
            let Ok(room) = room else {
                anyhow::bail!("the encoder stopped")
            };
            room.send((t, canvas));
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
                Some(m) if m - (k + 1) > 2 * opts.hold_slots && k >= catch_up_until => {
                    m - opts.hold_slots
                }
                _ => k + 1,
            });
        }
        let tiles: Vec<serde_json::Value> = (0..n)
            .map(|i| serde_json::json!({"id": tile_ids[i], "streaming": live[i], "newest": frames[i].keys().next_back()}))
            .collect();
        let mut st = shared.stats.lock().unwrap();
        st.tiles = tiles;
        st.rounds += 1;
    }
}

/// The environment variable a supervised aggregator reads its layout token from.
pub const LAYOUT_TOKEN_ENV: &str = "FCVM_FLEET_AGGREGATE_TOKEN";

/// Whether a `POST /layout` may change the layout. The listener also serves viewers, so
/// with a token configured (a supervised aggregator: the agent's or the coordinator's
/// child) the request must carry it as a bearer token; without one only a client on this
/// host may post.
fn layout_allowed(
    token: Option<&str>,
    authorization: Option<&str>,
    peer: Option<std::net::SocketAddr>,
) -> bool {
    match token {
        Some(t) => authorization
            .and_then(|a| a.strip_prefix("Bearer "))
            .is_some_and(|a| a.as_bytes() == t.as_bytes()),
        None => peer.is_some_and(|p| p.ip().to_canonical().is_loopback()),
    }
}

async fn layout_route(
    State(app): State<Arc<Shared>>,
    req: axum::extract::Request,
) -> axum::response::Response {
    let peer = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0);
    let authorization = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if !layout_allowed(
        app.layout_token.get().map(String::as_str),
        authorization,
        peer,
    ) {
        tracing::warn!(?peer, "a layout was refused");
        return (
            axum::http::StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "a layout needs the aggregator's token"})),
        )
            .into_response();
    }
    let body = match axum::body::to_bytes(req.into_body(), 1 << 20).await {
        Ok(b) => b,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
    };
    let l: Layout = match serde_json::from_slice(&body) {
        Ok(l) => l,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
    };
    tracing::info!(tiles = l.tiles.len(), region = ?l.region, "layout");
    app.set_layout(l);
    Json(serde_json::json!({"ok": true})).into_response()
}

async fn frames_route(ws: WebSocketUpgrade, State(app): State<Arc<Shared>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| viewer(socket, app))
}

/// The public listener's `/frames`: with admission keys in the layout, only a client whose
/// first message carries a direct token made with one of them (`{"admit": "<token>"}`, within
/// [`ADMIT_WAIT`]) gets the stream; anything else is closed with 1008. A token in the URL
/// would land in proxy and access logs, so the URL carries none.
async fn admitted_frames_route(
    ws: WebSocketUpgrade,
    State(app): State<Arc<Shared>>,
) -> axum::response::Response {
    // Before its first layout it cannot tell whether streams need a token: it serves none
    // (a socket opened now would keep its frames after a layout with keys arrives).
    let Some(keys) = app.layout.borrow().as_ref().map(|l| l.admit_keys.clone()) else {
        return (axum::http::StatusCode::SERVICE_UNAVAILABLE, "no layout yet").into_response();
    };
    // Nothing a viewer sends is larger than a height request, so an unadmitted client cannot
    // make it buffer more than this.
    ws.max_message_size(MAX_VIEWER_MESSAGE)
        .on_upgrade(move |mut socket| async move {
            if !keys.is_empty() {
                let first = tokio::time::timeout(ADMIT_WAIT, socket.recv()).await;
                let token = match first {
                    Ok(Some(Ok(Message::Text(t)))) => admit_message(&t),
                    _ => None,
                };
                if !admitted(&keys, token.as_deref()) {
                    let _ = socket
                        .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                            code: 1008,
                            reason: "a direct token is required".into(),
                        })))
                        .await;
                    return;
                }
            }
            viewer(socket, app).await
        })
        .into_response()
}

/// How long a stream that needs a direct token waits for the client's first message.
pub const ADMIT_WAIT: Duration = Duration::from_secs(5);

/// The largest message the public listener takes from a viewer.
pub const MAX_VIEWER_MESSAGE: usize = 4096;

/// The first message that admits a client to a stream: `{"admit": "<token>"}`.
pub fn admit_first_message(token: &str) -> String {
    serde_json::json!({ "admit": token }).to_string()
}

/// The direct token in a client's first message, if it is one.
pub fn admit_message(text: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()?
        .get("admit")?
        .as_str()
        .map(str::to_string)
}

/// Whether a client with `token` may stream, under the layout's admission keys (hex).
pub fn admitted(keys_hex: &[String], token: Option<&str>) -> bool {
    use crate::fleet::coord::admission::{direct_token_valid, AdmitKey};
    if keys_hex.is_empty() {
        return true;
    }
    let keys: Vec<AdmitKey> = keys_hex
        .iter()
        .filter_map(|h| AdmitKey::from_hex(h))
        .collect();
    token.is_some_and(|t| direct_token_valid(&keys, t, now_ms() / 1000))
}

/// A direct token for this aggregator's own tile subscriptions, made with the newest key.
pub(crate) fn own_token(keys_hex: &[String]) -> Option<String> {
    use crate::fleet::coord::admission::{AdmitKey, DIRECT_TOKEN_SECS};
    let key = AdmitKey::from_hex(keys_hex.first()?)?;
    Some(key.direct_token("aggregator", now_ms() / 1000 + DIRECT_TOKEN_SECS))
}

async fn viewer(socket: WebSocket, app: Arc<Shared>) {
    let (sink, mut stream) = socket.split();
    // Read and ignore what the viewer sends (a height request), so pings get answered.
    let reader = tokio::spawn(async move { while let Some(Ok(_)) = stream.next().await {} });
    feed(sink, &app).await;
    reader.abort();
}

/// Send a viewer the header and the frames since the last keyframe, then what follows. A
/// viewer that falls behind the broadcast starts again from the newest keyframe the same
/// way, with the header again if it changed meanwhile.
async fn feed<S>(mut sink: S, app: &Shared)
where
    S: futures_util::Sink<Message> + Unpin,
{
    let send = |m: &Out| match m {
        Out::Header(h) => Message::Text(h.clone().into()),
        Out::Frame { bytes, .. } => Message::Binary(bytes.clone().into()),
    };
    let (mut header, mut gop, mut rx) = app.join();
    let mut sent_header: Option<String> = None;
    loop {
        // From the snapshot: the header (unless this viewer has it), then the GOP.
        let mut want_key = true;
        if let Some(h) = header.take() {
            if sent_header.as_ref() != Some(&h) {
                if sink.send(Message::Text(h.clone().into())).await.is_err() {
                    return;
                }
                sent_header = Some(h);
            }
            for m in &gop {
                if sink.send(send(m)).await.is_err() {
                    return;
                }
                want_key = false;
            }
        }
        loop {
            match rx.recv().await {
                Ok(m) => {
                    match &*m {
                        Out::Frame { key, .. } => {
                            if want_key && !key {
                                continue;
                            }
                            want_key = false;
                        }
                        Out::Header(h) => sent_header = Some(h.clone()),
                    }
                    if sink.send(send(&m)).await.is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => break,
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
        // It fell behind: start again from the newest keyframe.
        (header, gop, rx) = app.join();
    }
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

/// What a public listener (`--tls-listen`) serves: the region stream and the health check,
/// never the layout or the stats.
pub fn public_router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/frames", get(admitted_frames_route))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(shared)
}

pub async fn cmd(args: FleetAggregateArgs) -> Result<()> {
    let codec: Arc<dyn Codec> = match args.codec {
        FleetAggregateCodec::Ffmpeg => Arc::new(codec::Ffmpeg {
            bin: args.ffmpeg.clone(),
        }),
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
    if let Some(t) = std::env::var(LAYOUT_TOKEN_ENV)
        .ok()
        .filter(|t| !t.is_empty())
    {
        shared.require_layout_token(t);
    }
    if let Some(path) = &args.layout {
        let l: Layout = serde_json::from_slice(&std::fs::read(path)?)
            .with_context(|| format!("reading {}", path.display()))?;
        shared.set_layout(l);
    }
    // The public listeners are bound before the private one, so a started aggregator
    // (the private `/healthz` answers) is serving every address it was given.
    let mut public = tokio::task::JoinSet::new();
    if !args.tls_listen.is_empty() {
        let file = args
            .tls_file
            .as_deref()
            .context("--tls-listen needs --tls-file")?;
        let certs = tls::CertFile::load(file)?;
        tokio::spawn(certs.clone().watch(tls::RELOAD));
        for addr in &args.tls_listen {
            let l = tls::TlsListener::bind(addr, certs.clone()).await?;
            eprintln!(
                "fleet aggregator serving /frames with TLS on {}",
                l.address()
            );
            let app = public_router(shared.clone());
            let addr = addr.clone();
            public.spawn(async move { (addr, axum::serve(l, app).await) });
        }
    }
    let cancel = CancellationToken::new();
    if let Some(var) = &args.push_url_env {
        let url = std::env::var(var)
            .ok()
            .filter(|u| !u.is_empty())
            .with_context(|| format!("--push-url-env names {var}, which is not set"))?;
        tokio::spawn(push::run(
            shared.clone(),
            args.ffmpeg.clone(),
            url,
            cancel.clone(),
        ));
    }
    tokio::spawn(run(shared.clone(), codec, opts, cancel.clone()));
    let listener = tokio::net::TcpListener::bind(&args.listen)
        .await
        .with_context(|| format!("listening on {}", args.listen))?;
    eprintln!("fleet aggregator listening on {}", listener.local_addr()?);
    let result = tokio::select! {
        r = axum::serve(
            listener,
            router(shared).into_make_service_with_connect_info::<std::net::SocketAddr>(),
        ) => r.map_err(anyhow::Error::from),
        Some(r) = public.join_next() => match r {
            Ok((addr, Ok(()))) => Err(anyhow::anyhow!("the listener on {addr} stopped")),
            Ok((addr, Err(e))) => Err(anyhow::Error::from(e).context(format!("serving on {addr}"))),
            Err(e) => Err(anyhow::Error::from(e)),
        },
    };
    cancel.cancel();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A layout with nothing to stream (every tile black) drops the tile-event channel's
    /// last sender before the loop starts; the loop must still sleep on its timers.
    #[tokio::test]
    async fn a_region_with_no_streaming_tile_does_not_spin() {
        let shared = Shared::new();
        let layout = Layout {
            epoch_unix_ms: now_ms(),
            image_aspect: 16.0 / 9.0,
            region: Rect {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
            },
            tiles: vec![TileSpec {
                id: "t0".into(),
                rect: Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 1.0,
                    h: 1.0,
                },
                url: None,
            }],
            generation: 1,
            from_frame: 0,
            admit_keys: Vec::new(),
        };
        let opts = Options {
            image_height: 36,
            ..Options::default()
        };
        let (_publish_tx, publish) = watch::channel(true);
        let (_until_tx, until) = watch::channel(i64::MAX);
        let gate = Gate {
            publish,
            until,
            ready: None,
        };
        let cancel = CancellationToken::new();
        let task = tokio::spawn(pipeline(
            shared.clone(),
            Arc::new(codec::Raw),
            opts,
            layout,
            gate,
            cancel.clone(),
        ));
        tokio::time::sleep(Duration::from_millis(500)).await;
        cancel.cancel();
        task.await.unwrap().unwrap();
        let s = shared.stats.lock().unwrap();
        // The 5 ms tick, 30 encoded frames a second and the encoder's outputs wake it about
        // 160 times in 500 ms; a loop that does not wait runs many thousands of rounds.
        assert!(s.rounds < 1000, "{} compositor rounds in 500 ms", s.rounds);
        assert!(s.frames > 0, "the black region was not encoded");
    }

    /// A layout of one tile with nothing to stream: a black region.
    fn black(generation: u64, epoch_unix_ms: u64, from_frame: i64) -> Layout {
        let whole = Rect {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
        };
        Layout {
            epoch_unix_ms,
            image_aspect: 16.0 / 9.0,
            region: whole,
            tiles: vec![TileSpec {
                id: "t0".into(),
                rect: whole,
                url: None,
            }],
            generation,
            from_frame,
            admit_keys: Vec::new(),
        }
    }

    fn small() -> Options {
        Options {
            image_height: 36,
            ..Options::default()
        }
    }

    /// The scene time of the newest frame the aggregator has encoded.
    fn newest_t(shared: &Shared) -> Option<f64> {
        shared.stats.lock().unwrap().recent.back().map(|(_, f)| f.t)
    }

    async fn until(what: &str, mut ok: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !ok() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{what} within 10 s"));
    }

    /// The publishing pipeline stops (here its encoder refuses to start): it is started
    /// again, instead of the region going dark until the next layout.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_publishing_pipeline_that_stops_is_started_again() {
        let shared = Shared::new();
        shared.set_layout(black(1, now_ms(), 0));
        let cancel = CancellationToken::new();
        tokio::spawn(run(
            shared.clone(),
            Arc::new(codec::Narrow::new(vec![0])),
            small(),
            cancel.clone(),
        ));
        until("a frame from the pipeline started again", || {
            shared.stats.lock().unwrap().frames > 0
        })
        .await;
        cancel.cancel();
    }

    /// A publishing pipeline whose encoder stops while a tile is connected and sends
    /// nothing: the pipeline started again must not leave the old one's tile reader
    /// connected. Each restart would otherwise keep one more connection and decoder.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pipeline_started_again_leaves_no_tile_reader_behind() {
        use axum::extract::ws::WebSocketUpgrade;
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        // Connections made, and connections open.
        let made = Arc::new(AtomicUsize::new(0));
        let open = Arc::new(AtomicUsize::new(0));
        let (m2, o2) = (made.clone(), open.clone());
        let app = Router::new().route(
            "/frames",
            get(move |ws: WebSocketUpgrade| {
                let (made, open) = (m2.clone(), o2.clone());
                async move {
                    ws.on_upgrade(move |mut s| async move {
                        made.fetch_add(1, SeqCst);
                        open.fetch_add(1, SeqCst);
                        // A tile that is up and sends nothing; read until the reader leaves.
                        while let Some(Ok(_)) = s.recv().await {}
                        open.fetch_sub(1, SeqCst);
                    })
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/frames", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let mut layout = black(1, now_ms(), 0);
        layout.tiles[0].url = Some(url);
        let shared = Shared::new();
        shared.set_layout(layout);
        let codec = codec::Narrow {
            stop: vec![0],
            ..codec::Narrow::new(vec![])
        };
        let cancel = CancellationToken::new();
        tokio::spawn(run(
            shared.clone(),
            Arc::new(codec),
            small(),
            cancel.clone(),
        ));
        until("the pipeline started again to connect", || {
            made.load(SeqCst) == 2
        })
        .await;
        until("one tile connection left open", || open.load(SeqCst) == 1).await;
        cancel.cancel();
    }

    /// A new generation's pipeline fails while it warms (its encoder refuses to start): the
    /// old pipeline, which was told to stop composing at the new generation's frame, keeps
    /// composing past it instead of freezing the region.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_new_generation_that_fails_leaves_the_old_composite_running() {
        let shared = Shared::new();
        let epoch = now_ms();
        shared.set_layout(black(1, epoch, 0));
        let cancel = CancellationToken::new();
        tokio::spawn(run(
            shared.clone(),
            Arc::new(codec::Narrow::new(vec![1])),
            small(),
            cancel.clone(),
        ));
        until("a frame of the first generation", || {
            newest_t(&shared).is_some()
        })
        .await;
        // The new generation takes effect half a second from now.
        let from = slot((now_ms() - epoch) as f64 / 1000.0, 30) + 15;
        shared.set_layout(black(2, epoch, from));
        let past = (from + 30) as f64 / 30.0;
        until(
            "a frame a second past the failed generation's frame",
            || newest_t(&shared).is_some_and(|t| t >= past),
        )
        .await;
        cancel.cancel();
    }

    /// The compositor has a backlog of overdue slots (here held back by `until`, as a swap
    /// does) and composes them in one round. Each picture waits for room in the encoder's
    /// input, which frees only as the encoder's output is taken: the round must take it
    /// meanwhile, or both fill and the pipeline stops for good.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_backlog_of_overdue_slots_does_not_stall_the_encoder() {
        let shared = Shared::new();
        let (_publish_tx, publish) = watch::channel(true);
        let (until_tx, until) = watch::channel(i64::MIN);
        let gate = Gate {
            publish,
            until,
            ready: None,
        };
        let cancel = CancellationToken::new();
        let task = tokio::spawn(pipeline(
            shared.clone(),
            Arc::new(codec::Narrow::new(vec![])),
            small(),
            black(1, now_ms(), 0),
            gate,
            cancel.clone(),
        ));
        // About 30 slots fall due while nothing may be composed.
        tokio::time::sleep(Duration::from_secs(1)).await;
        until_tx.send_replace(i64::MAX);
        tokio::time::sleep(Duration::from_millis(500)).await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the pipeline is stuck composing its backlog")
            .unwrap()
            .unwrap();
        let frames = shared.stats.lock().unwrap().frames;
        assert!(frames >= 30, "{frames} frames encoded");
    }

    /// A viewer that falls behind the broadcast (here: it reads nothing while 300 frames are
    /// published, more than the broadcast keeps) starts again from the newest keyframe,
    /// from the GOP the aggregator holds, instead of waiting for the next keyframe (two
    /// seconds at the encoder's interval, never here). The header it has is not sent again.
    #[tokio::test]
    async fn a_viewer_that_falls_behind_starts_again_from_the_newest_keyframe() {
        let shared = Shared::new();
        shared.publish(Out::Header("header".into()));
        shared.publish(Out::Frame {
            key: true,
            bytes: vec![0],
        });
        let (tx, mut rx) = mpsc::channel::<Message>(1);
        let sink = Box::pin(futures_util::sink::unfold(
            tx,
            |tx, m: Message| async move {
                tx.send(m).await.map_err(|_| ())?;
                Ok::<_, ()>(tx)
            },
        ));
        let s2 = shared.clone();
        tokio::spawn(async move { feed(sink, &s2).await });
        async fn recv(
            rx: &mut mpsc::Receiver<Message>,
        ) -> Result<Option<Message>, tokio::time::error::Elapsed> {
            tokio::time::timeout(Duration::from_secs(5), rx.recv()).await
        }
        assert_eq!(
            recv(&mut rx).await.unwrap().unwrap(),
            Message::Text("header".into())
        );
        assert_eq!(
            recv(&mut rx).await.unwrap().unwrap(),
            Message::Binary(vec![0].into())
        );
        for i in 1..=300u32 {
            let mut bytes = vec![1];
            bytes.extend_from_slice(&i.to_le_bytes());
            shared.publish(Out::Frame { key: false, bytes });
        }
        loop {
            let m = recv(&mut rx)
                .await
                .expect("the keyframe again, without a new one published")
                .unwrap();
            match m {
                Message::Binary(b) if b[0] == 0 => break,
                Message::Binary(_) => {}
                other => panic!("{other:?} before the keyframe"),
            }
        }
    }

    /// A supervised aggregator takes a layout only with its token: its listener also serves
    /// viewers, who must not be able to change what it composes.
    #[tokio::test]
    async fn a_layout_needs_the_aggregators_token() {
        let shared = Shared::new();
        shared.require_layout_token("test-token".into());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/layout", l.local_addr().unwrap());
        let s2 = shared.clone();
        tokio::spawn(async move {
            axum::serve(
                l,
                router(s2).into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap()
        });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let layout = black(1, 1_000, 0);
        let post = |auth: Option<&str>| {
            let mut r = http.post(&url).json(&layout);
            if let Some(a) = auth {
                r = r.header("authorization", a);
            }
            r.send()
        };
        assert_eq!(post(None).await.unwrap().status(), 403);
        assert_eq!(post(Some("Bearer wrong")).await.unwrap().status(), 403);
        assert!(
            shared.layout.borrow().is_none(),
            "a refused layout was taken"
        );
        assert_eq!(post(Some("Bearer test-token")).await.unwrap().status(), 200);
        assert_eq!(*shared.layout.borrow(), Some(layout));
    }

    /// Without a token (an aggregator an operator runs by hand) only this host may post.
    #[test]
    fn without_a_token_only_this_host_may_post_a_layout() {
        let at = |s: &str| Some(s.parse::<std::net::SocketAddr>().unwrap());
        assert!(layout_allowed(None, None, at("127.0.0.1:5000")));
        assert!(layout_allowed(None, None, at("[::1]:5000")));
        assert!(layout_allowed(None, None, at("[::ffff:127.0.0.1]:5000")));
        assert!(!layout_allowed(None, None, at("203.0.113.5:5000")));
        assert!(!layout_allowed(None, None, None));
        assert!(!layout_allowed(Some("t"), None, at("127.0.0.1:5000")));
        assert!(layout_allowed(
            Some("t"),
            Some("Bearer t"),
            at("203.0.113.5:5000")
        ));
    }
}
