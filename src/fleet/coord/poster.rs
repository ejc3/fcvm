//! The poster frame: the latest converged still of the whole image, served at
//! `/poster.jpg` for link previews and the page's first paint.
//!
//! Once per camera stop, after the picture has had [`CONVERGE_SECS`] to settle, the
//! coordinator subscribes to every aggregator (each fcvm host's band and the EC2 half),
//! decodes the first frame at or after that scene time, places each region at its spot in
//! the whole image and encodes one JPEG. Every aggregator draws the same frame times, so
//! the regions are one frame; the camera moves slowly at a stop, so a region that skipped
//! that frame and gave the next one is off by one frame of a slow move. A stop whose
//! capture does not finish before the flight starts keeps the previous poster.

use super::Handle;
use crate::fleet::aggregate::codec::{Codec, DecoderIo};
use crate::fleet::aggregate::compose::{blit, Geometry, Picture, BLACK};
use crate::fleet::aggregate::frame::{Frame, Header};
use crate::fleet::now_ms;
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;

/// Scene seconds into a stop before its frames count as settled (stops last 9 s or more,
/// `fleet_core::director::Viewpoint::stay_secs`).
pub const CONVERGE_SECS: f64 = 5.5;
/// Subscribe this long before the capture time, so the join's keyframe and the frames
/// after it are decoded by then.
const LEAD_SECS: f64 = 1.5;

/// What the coordinator says about the current stop.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// Identifies the stop: the director's step and arrival time.
    pub step: u64,
    pub arrive_t: f64,
    pub hold_until_t: f64,
    pub viewpoint: String,
    pub epoch_unix_ms: u64,
    /// Aggregator frame WebSockets, reachable from the coordinator.
    pub sources: Vec<String>,
}

/// One encoded still.
#[derive(Debug, Clone)]
pub struct Still {
    pub content_type: &'static str,
    pub bytes: Vec<u8>,
    pub viewpoint: String,
    pub t: f64,
    pub taken_unix_ms: u64,
}

/// How a composed picture becomes bytes.
pub enum StillEncoder {
    /// JPEG through the ffmpeg CLI.
    Jpeg { ffmpeg: String },
    /// Binary PPM (for tests: no ffmpeg needed and the pixels are easy to read back).
    Ppm,
}

#[derive(Default)]
pub struct Poster {
    latest: Mutex<Option<Arc<Still>>>,
}

impl Poster {
    pub fn latest(&self) -> Option<Arc<Still>> {
        self.latest.lock().unwrap().clone()
    }

    fn set(&self, s: Still) {
        *self.latest.lock().unwrap() = Some(Arc::new(s));
    }
}

/// BT.709 limited-range YUV to 8-bit RGB.
pub fn rgb_of(y: u8, u: u8, v: u8) -> [u8; 3] {
    let y = (y as f64 - 16.0) * 255.0 / 219.0;
    let u = (u as f64 - 128.0) * 255.0 / 224.0;
    let v = (v as f64 - 128.0) * 255.0 / 224.0;
    let r = y + 1.5748 * v;
    let g = y - 0.1873 * u - 0.4681 * v;
    let b = y + 1.8556 * u;
    let c = |x: f64| x.round().clamp(0.0, 255.0) as u8;
    [c(r), c(g), c(b)]
}

/// Interleaved RGB24 of a planar YUV 4:4:4 picture.
pub fn to_rgb(p: &Picture) -> Vec<u8> {
    let plane = p.width as usize * p.height as usize;
    let mut out = Vec::with_capacity(plane * 3);
    for i in 0..plane {
        out.extend_from_slice(&rgb_of(p.data[i], p.data[plane + i], p.data[2 * plane + i]));
    }
    out
}

impl StillEncoder {
    pub async fn encode(&self, p: &Picture) -> Result<(&'static str, Vec<u8>)> {
        let rgb = to_rgb(p);
        match self {
            StillEncoder::Ppm => {
                let mut out = format!("P6\n{} {}\n255\n", p.width, p.height).into_bytes();
                out.extend_from_slice(&rgb);
                Ok(("image/x-portable-pixmap", out))
            }
            StillEncoder::Jpeg { ffmpeg } => {
                let size = format!("{}x{}", p.width, p.height);
                let mut child = tokio::process::Command::new(ffmpeg)
                    .args([
                        "-hide_banner",
                        "-loglevel",
                        "error",
                        "-f",
                        "rawvideo",
                        "-pix_fmt",
                        "rgb24",
                        "-s",
                        &size,
                        "-i",
                        "pipe:0",
                        "-frames:v",
                        "1",
                        "-q:v",
                        "3",
                        "-f",
                        "image2pipe",
                        "-c:v",
                        "mjpeg",
                        "pipe:1",
                    ])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::inherit())
                    .kill_on_drop(true)
                    .spawn()
                    .with_context(|| format!("starting {ffmpeg}"))?;
                let mut stdin = child.stdin.take().context("ffmpeg stdin")?;
                let mut stdout = child.stdout.take().context("ffmpeg stdout")?;
                let write = async move {
                    stdin.write_all(&rgb).await?;
                    stdin.shutdown().await
                };
                let mut jpeg = Vec::new();
                let (w, r) = tokio::join!(write, stdout.read_to_end(&mut jpeg));
                w.context("writing the poster to ffmpeg")?;
                r.context("reading the JPEG from ffmpeg")?;
                let status = child.wait().await?;
                if !status.success() || !jpeg.starts_with(&[0xff, 0xd8]) {
                    bail!("ffmpeg did not produce a JPEG ({status})");
                }
                Ok(("image/jpeg", jpeg))
            }
        }
    }
}

/// The first decoded picture of one aggregator stream at or after scene time `at`.
async fn grab(url: &str, codec: &dyn Codec, at: f64) -> Result<(Header, Picture)> {
    let (mut ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .with_context(|| format!("connecting to {url}"))?;
    let mut header: Option<Header> = None;
    let mut dec: Option<DecoderIo> = None;
    let mut want_key = true;
    loop {
        tokio::select! {
            msg = ws.next() => {
                let Some(msg) = msg else { bail!("{url} closed before a frame at t={at:.2}") };
                match msg? {
                    Message::Text(t) => {
                        let h: Header = serde_json::from_str(&t)?;
                        dec = Some(codec.decoder(h.width, h.height)?);
                        header = Some(h);
                        want_key = true;
                    }
                    Message::Binary(b) => {
                        let Some((tx, _)) = dec.as_ref() else { continue };
                        let f = Frame::parse(&b)?;
                        if want_key && !f.key {
                            continue;
                        }
                        want_key = false;
                        tx.send((f.t, f.au)).await.context("the decoder stopped")?;
                    }
                    Message::Close(_) => bail!("{url} closed before a frame at t={at:.2}"),
                    _ => {}
                }
            }
            out = async { dec.as_mut().unwrap().1.recv().await }, if dec.is_some() => {
                let Some((t, pic)) = out else { bail!("the decoder stopped") };
                if t >= at {
                    return Ok((header.expect("a decoder implies a header"), pic));
                }
            }
        }
    }
}

/// Grab every source at `at` and place them in one picture of the whole image.
pub async fn compose(sources: &[String], codec: &dyn Codec, at: f64) -> Result<Picture> {
    let grabs = futures_util::future::join_all(sources.iter().map(|u| grab(u, codec, at))).await;
    let mut parts = Vec::new();
    for (u, g) in sources.iter().zip(grabs) {
        match g {
            Ok(p) => parts.push(p),
            Err(e) => {
                tracing::info!(url = %u, error = %format!("{e:#}"), "poster: a region is missing")
            }
        }
    }
    let Some((h0, _)) = parts.first() else {
        bail!("no aggregator delivered a frame");
    };
    let (fw, fh) = (h0.full_width.max(h0.width), h0.full_height.max(h0.height));
    let g = Geometry {
        full_width: fw,
        full_height: fh,
        x: 0,
        y: 0,
        width: fw,
        height: fh,
    };
    let mut canvas = Picture::filled(fw, fh, BLACK);
    for (h, pic) in &parts {
        blit(&mut canvas, &g, h, pic);
    }
    Ok(canvas)
}

/// Take one poster per stop, for as long as the coordinator runs.
pub async fn run(
    handle: Handle,
    poster: Arc<Poster>,
    codec: Arc<dyn Codec>,
    encoder: StillEncoder,
) {
    let mut done: Option<(u64, u64)> = None;
    let mut every = tokio::time::interval(Duration::from_millis(250));
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        let Some(plan) = handle.poster_plan().await else {
            if handle.cmds.is_closed() {
                return;
            }
            continue;
        };
        let key = (plan.step, plan.arrive_t.to_bits());
        if done == Some(key) || plan.sources.is_empty() {
            continue;
        }
        let at = plan.arrive_t + CONVERGE_SECS;
        let now_t = (now_ms() as f64 - plan.epoch_unix_ms as f64) / 1000.0;
        if now_t < at - LEAD_SECS {
            continue;
        }
        done = Some(key);
        if now_t >= plan.hold_until_t {
            continue;
        }
        let budget = Duration::from_secs_f64((plan.hold_until_t - now_t).max(0.0) + 0.5);
        let started = now_ms();
        let r = tokio::time::timeout(budget, async {
            let pic = compose(&plan.sources, codec.as_ref(), at).await?;
            let (content_type, bytes) = encoder.encode(&pic).await?;
            anyhow::Ok((pic, content_type, bytes))
        })
        .await;
        match r {
            Ok(Ok((pic, content_type, bytes))) => {
                tracing::info!(
                    viewpoint = %plan.viewpoint,
                    width = pic.width,
                    height = pic.height,
                    bytes = bytes.len(),
                    ms = now_ms() - started,
                    "poster taken"
                );
                poster.set(Still {
                    content_type,
                    bytes,
                    viewpoint: plan.viewpoint.clone(),
                    t: at,
                    taken_unix_ms: now_ms(),
                });
            }
            Ok(Err(e)) => {
                tracing::warn!(viewpoint = %plan.viewpoint, error = %format!("{e:#}"), "poster not taken")
            }
            Err(_) => {
                tracing::warn!(viewpoint = %plan.viewpoint, "poster not taken before the stop ended")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limited_range_extremes_map_to_full_range_rgb() {
        assert_eq!(rgb_of(16, 128, 128), [0, 0, 0]);
        assert_eq!(rgb_of(235, 128, 128), [255, 255, 255]);
        // BT.709 limited red: Y 63, U 102, V 240.
        let [r, g, b] = rgb_of(63, 102, 240);
        assert!(r >= 250 && g <= 5 && b <= 5, "{r} {g} {b}");
    }
}
