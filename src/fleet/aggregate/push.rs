//! Pushing the aggregator's output to a live ingest (the broadcast: Amazon IVS over RTMPS).
//!
//! The pusher reads the same frames viewers get, starting at a keyframe, and writes them as
//! FLV to an `ffmpeg` child that copies them to the ingest URL. Each frame's FLV timestamp is
//! its scene time `t` (milliseconds after the first pushed frame), so a skipped slot is a
//! gap, never a drift. Every access unit carries an SEI message (user data unregistered,
//! [`SEI_UUID`]) whose payload is the JSON `{"t": <scene seconds>, "e": <scene epoch, unix
//! ms>}`: a player reads the `t` of the frame it is showing, which is what the page lines its
//! overlay up with (fleet/PROTOCOL.md, "Broadcast"). A child that exits is started again
//! after 1 s, doubling to 30 s, from the next keyframe.

use super::frame::{nal_units, Frame};
use super::{Out, Shared};
use anyhow::{Context, Result};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// The UUID of the SEI message that carries a frame's scene time. Fixed: players look for it.
pub const SEI_UUID: [u8; 16] = [
    0x6a, 0x0e, 0x3c, 0x5d, 0x9b, 0x41, 0x4f, 0x2e, 0x8d, 0x77, 0x1c, 0x52, 0xf3, 0x0a, 0xb6, 0x19,
];

/// The SEI payload of one frame.
pub fn sei_payload(t: f64, epoch_unix_ms: u64) -> Vec<u8> {
    // Microsecond precision is enough to name a 30 fps slot and keeps the payload short.
    format!(r#"{{"t":{:.6},"e":{epoch_unix_ms}}}"#, t).into_bytes()
}

/// Insert emulation prevention bytes into an RBSP (H.264 7.4.1): a 0x03 after any two zero
/// bytes that are followed by a byte of 0x03 or less.
pub fn escape_rbsp(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + rbsp.len() / 64 + 1);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// Remove emulation prevention bytes: the inverse of [`escape_rbsp`].
pub fn unescape_rbsp(ebsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ebsp.len());
    let mut zeros = 0;
    for &b in ebsp {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// One SEI NAL unit (without a start code) holding a user data unregistered message.
pub fn sei_nal(uuid: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let mut rbsp = vec![5u8]; // payload type 5: user data unregistered
    let mut size = uuid.len() + data.len();
    while size >= 255 {
        rbsp.push(255);
        size -= 255;
    }
    rbsp.push(size as u8);
    rbsp.extend_from_slice(uuid);
    rbsp.extend_from_slice(data);
    rbsp.push(0x80); // rbsp_trailing_bits
    let mut nal = vec![0x06];
    nal.extend(escape_rbsp(&rbsp));
    nal
}

/// The user data unregistered messages of an SEI NAL unit (header byte included), as
/// (uuid, data) pairs.
pub fn parse_sei(nal: &[u8]) -> Vec<([u8; 16], Vec<u8>)> {
    let mut found = Vec::new();
    if nal.first().map(|b| b & 0x1f) != Some(6) {
        return found;
    }
    let b = unescape_rbsp(&nal[1..]);
    let mut i = 0;
    // Each message: type and size as runs of 255 plus a last byte; stop at the trailing bits.
    while i < b.len() && b[i] != 0x80 {
        let mut kind = 0usize;
        while i < b.len() && b[i] == 255 {
            kind += 255;
            i += 1;
        }
        let Some(&k) = b.get(i) else { break };
        kind += k as usize;
        i += 1;
        let mut size = 0usize;
        while i < b.len() && b[i] == 255 {
            size += 255;
            i += 1;
        }
        let Some(&s) = b.get(i) else { break };
        size += s as usize;
        i += 1;
        let Some(payload) = b.get(i..i + size) else {
            break;
        };
        if kind == 5 && size >= 16 {
            found.push((payload[..16].try_into().unwrap(), payload[16..].to_vec()));
        }
        i += size;
    }
    found
}

/// The access unit as length-prefixed NAL units with the SEI placed before its first slice,
/// and its SPS and PPS (the last of each) when it carries them.
pub struct Unit {
    pub avcc: Vec<u8>,
    pub sps: Option<Vec<u8>>,
    pub pps: Option<Vec<u8>>,
}

pub fn to_avcc_with_sei(au: &[u8], sei: &[u8]) -> Unit {
    let mut u = Unit {
        avcc: Vec::with_capacity(au.len() + sei.len() + 16),
        sps: None,
        pps: None,
    };
    let put = |out: &mut Vec<u8>, n: &[u8]| {
        out.extend_from_slice(&(n.len() as u32).to_be_bytes());
        out.extend_from_slice(n);
    };
    let mut sei_done = false;
    for n in nal_units(au) {
        let Some(&h) = n.first() else { continue };
        match h & 0x1f {
            // Delimiters are dropped: FLV frames the unit itself.
            9 => continue,
            7 => u.sps = Some(n.to_vec()),
            8 => u.pps = Some(n.to_vec()),
            1 | 5 if !sei_done => {
                put(&mut u.avcc, sei);
                sei_done = true;
            }
            _ => {}
        }
        put(&mut u.avcc, n);
    }
    u
}

/// The AVCDecoderConfigurationRecord of one SPS and one PPS.
pub fn avc_config(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut c = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
    c.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    c.extend_from_slice(sps);
    c.push(1);
    c.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    c.extend_from_slice(pps);
    c
}

/// An FLV stream: the video, and a silent audio track beside it.
#[derive(Default)]
pub struct FlvWriter {
    config: Option<Vec<u8>>,
    started: bool,
    /// Silent audio frames written so far.
    audio_frames: u64,
}

/// The FLV file header (audio and video) and the first PreviousTagSize.
pub const FLV_HEADER: [u8; 13] = [b'F', b'L', b'V', 1, 5, 0, 0, 0, 9, 0, 0, 0, 0];

/// The silent track: AAC-LC, 48 kHz, mono. Its AudioSpecificConfig, and one frame of
/// silence (1024 samples), the bytes ffmpeg's AAC encoder writes for every frame of digital
/// silence after the first. IVS serves a stream without audio late: measured on one
/// composite, frames reached players 5.2 s after their scene time without audio and 1.8 s
/// with this track (medians, five 60 s runs each).
pub const AAC_CONFIG: [u8; 2] = [0x11, 0x88];
pub const AAC_SILENCE: [u8; 4] = [0x01, 0x18, 0x20, 0x07];
const AAC_RATE: u64 = 48_000;
const AAC_FRAME: u64 = 1024;

/// The time of audio frame `n`, ms.
fn audio_ms(n: u64) -> u64 {
    n * AAC_FRAME * 1000 / AAC_RATE
}

fn tag(out: &mut Vec<u8>, kind: u8, ts_ms: u32, body: &[&[u8]]) {
    let size: usize = body.iter().map(|b| b.len()).sum();
    out.push(kind);
    out.extend_from_slice(&(size as u32).to_be_bytes()[1..]);
    out.extend_from_slice(&ts_ms.to_be_bytes()[1..]);
    out.push((ts_ms >> 24) as u8);
    out.extend_from_slice(&[0, 0, 0]);
    for b in body {
        out.extend_from_slice(b);
    }
    out.extend_from_slice(&((11 + size) as u32).to_be_bytes());
}

fn video_tag(out: &mut Vec<u8>, ts_ms: u32, key: bool, packet_type: u8, body: &[u8]) {
    // Frame type and codec, packet type, composition time 0 (no B-frames).
    let head = [if key { 0x17 } else { 0x27 }, packet_type, 0, 0, 0];
    tag(out, 9, ts_ms, &[&head, body]);
}

fn audio_tag(out: &mut Vec<u8>, ts_ms: u32, packet_type: u8, body: &[u8]) {
    // AAC, 44 kHz flag (always set for AAC), 16-bit, stereo flag (always set for AAC).
    tag(out, 8, ts_ms, &[&[0xAF, packet_type], body]);
}

/// An `onMetaData` script tag naming the video's size, rate and codec (AVC) and the audio's
/// (AAC). A demuxer that reads it knows the streams without probing them.
pub fn metadata_tag(width: u32, height: u32, fps: f64) -> Vec<u8> {
    let mut body = vec![2u8];
    body.extend_from_slice(&10u16.to_be_bytes());
    body.extend_from_slice(b"onMetaData");
    let entries: [(&str, f64); 6] = [
        ("width", width as f64),
        ("height", height as f64),
        ("framerate", fps),
        ("videocodecid", 7.0),
        ("audiocodecid", 10.0),
        ("audiosamplerate", AAC_RATE as f64),
    ];
    body.push(8);
    body.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for (k, v) in entries {
        body.extend_from_slice(&(k.len() as u16).to_be_bytes());
        body.extend_from_slice(k.as_bytes());
        body.push(0);
        body.extend_from_slice(&v.to_be_bytes());
    }
    body.extend_from_slice(&[0, 0, 9]);
    let mut out = Vec::new();
    tag(&mut out, 18, 0, &[&body]);
    out
}

impl FlvWriter {
    /// The bytes for one unit at `ts_ms`: the file header, the metadata and the audio's
    /// sequence header first; a video sequence header whenever a keyframe brings a
    /// configuration that differs from the last one sent; then silent audio up to `ts_ms`, so
    /// the two tracks advance together. A stream must start at a keyframe with an SPS and a
    /// PPS; until then nothing is written. `size` is the picture's width and height.
    pub fn unit(&mut self, ts_ms: u32, key: bool, u: &Unit, size: (u32, u32)) -> Vec<u8> {
        let mut out = Vec::new();
        if let (true, Some(sps), Some(pps)) = (key, &u.sps, &u.pps) {
            let c = avc_config(sps, pps);
            if self.config.as_ref() != Some(&c) {
                if !self.started {
                    out.extend_from_slice(&FLV_HEADER);
                    out.extend(metadata_tag(size.0, size.1, 30.0));
                    audio_tag(&mut out, ts_ms, 0, &AAC_CONFIG);
                    self.audio_frames = 0;
                    self.started = true;
                }
                video_tag(&mut out, ts_ms, true, 0, &c);
                self.config = Some(c);
            }
        }
        if !self.started {
            return out;
        }
        while audio_ms(self.audio_frames) <= ts_ms as u64 {
            audio_tag(
                &mut out,
                audio_ms(self.audio_frames) as u32,
                1,
                &AAC_SILENCE,
            );
            self.audio_frames += 1;
        }
        video_tag(&mut out, ts_ms, key, 1, &u.avcc);
        out
    }
}

/// Frames as FLV bytes with timestamps from their scene time: the first frame written is 0
/// ms, and a frame at or before the previous one's time is dropped.
pub struct Timeline {
    flv: FlvWriter,
    t0: Option<f64>,
    last_ms: Option<u32>,
    waiting_key: bool,
}

impl Default for Timeline {
    fn default() -> Self {
        Timeline {
            flv: FlvWriter::default(),
            t0: None,
            last_ms: None,
            waiting_key: true,
        }
    }
}

impl Timeline {
    /// Frames were lost (the reader fell behind): drop deltas until the next keyframe.
    pub fn lost_frames(&mut self) {
        self.waiting_key = true;
    }

    pub fn frame(&mut self, f: &Frame, epoch_unix_ms: u64) -> Vec<u8> {
        if self.waiting_key && !f.key {
            return Vec::new();
        }
        let t0 = *self.t0.get_or_insert(f.t);
        let ms = ((f.t - t0) * 1000.0).round();
        if !(0.0..=u32::MAX as f64).contains(&ms) {
            return Vec::new();
        }
        let ms = ms as u32;
        if self.last_ms.is_some_and(|l| ms <= l) {
            return Vec::new();
        }
        let unit = to_avcc_with_sei(&f.au, &sei_nal(&SEI_UUID, &sei_payload(f.t, epoch_unix_ms)));
        let out = self
            .flv
            .unit(ms, f.key, &unit, (f.width as u32, f.height as u32));
        if !out.is_empty() {
            self.last_ms = Some(ms);
            self.waiting_key = false;
        }
        out
    }
}

/// The `ffmpeg` arguments that copy an FLV stream on stdin (video and its silent audio) to
/// `url`.
pub fn ffmpeg_args(url: &str) -> Vec<String> {
    [
        "-hide_banner",
        "-loglevel",
        "warning",
        // The metadata tag names the stream, so nothing is probed or held back. (`-fflags
        // nobuffer` drops what the probe read: the first GOP.)
        "-probesize",
        "32",
        "-analyzeduration",
        "0",
        "-f",
        "flv",
        "-i",
        "pipe:0",
        "-c",
        "copy",
        "-flvflags",
        "no_duration_filesize",
        "-f",
        "flv",
        url,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// How long a write to the child may make no progress before the child is replaced: the
/// ingest stalled, ffmpeg stopped reading, and nothing else would notice.
pub const WRITE_STALL: Duration = Duration::from_secs(5);

/// Push the aggregator's output to `url` until `cancel`.
pub async fn run(shared: Arc<Shared>, ffmpeg: String, url: String, cancel: CancellationToken) {
    let mut backoff = Duration::from_secs(1);
    while !cancel.is_cancelled() {
        let started = Instant::now();
        match once(&shared, &ffmpeg, &url, &cancel).await {
            Ok(()) => return,
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "broadcast push stopped"),
        }
        if started.elapsed() >= Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn once(shared: &Shared, ffmpeg: &str, url: &str, cancel: &CancellationToken) -> Result<()> {
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.args(ffmpeg_args(url))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    // kill_on_drop cannot run when the aggregator itself is killed (the supervisor's stop is
    // a SIGKILL), and an ffmpeg blocked on a stalled ingest does not exit when its stdin
    // closes: the kernel ends it with its parent.
    super::child::arm_pdeathsig(cmd.as_std_mut());
    let mut child = cmd.spawn().with_context(|| format!("starting {ffmpeg}"))?;
    let mut stdin = child.stdin.take().context("ffmpeg stdin")?;
    let mut rx = shared.out.subscribe();
    let mut line = Timeline::default();
    let mut pushed = 0u64;
    tracing::info!("broadcast push started");
    loop {
        let m = tokio::select! {
            _ = cancel.cancelled() => {
                drop(stdin);
                let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
                return Ok(());
            }
            status = child.wait() => anyhow::bail!("ffmpeg exited ({status:?}) after {pushed} frames"),
            m = rx.recv() => m,
        };
        let m = match m {
            Ok(m) => m,
            // Fell behind: the timeline drops frames until the next keyframe it can use.
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(skipped = n, "broadcast push fell behind");
                line.lost_frames();
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => return Ok(()),
        };
        let Out::Frame { bytes, .. } = &*m else {
            continue;
        };
        let f = Frame::parse(bytes)?;
        let epoch = shared
            .layout
            .borrow()
            .as_ref()
            .map_or(0, |l| l.epoch_unix_ms);
        let out = line.frame(&f, epoch);
        if out.is_empty() {
            continue;
        }
        // Bounded and cancellable: a stalled ingest stops ffmpeg reading, and a write that
        // waited on it would hold up both cancellation and the restart.
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            status = child.wait() => anyhow::bail!("ffmpeg exited ({status:?}) after {pushed} frames"),
            r = tokio::time::timeout(WRITE_STALL, stdin.write_all(&out)) => match r {
                Ok(r) => r.context("writing to ffmpeg")?,
                Err(_) => anyhow::bail!("ffmpeg took no data for {WRITE_STALL:?}: the ingest stalled"),
            },
        }
        pushed += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rbsp_escaping_round_trips_and_breaks_start_codes() {
        let raw = [0u8, 0, 1, 0, 0, 0, 0, 0, 3, 0, 0, 2, 7, 0, 0];
        let e = escape_rbsp(&raw);
        // No start code (00 00 01) or 00 00 00 survives inside the escaped bytes.
        assert!(!e.windows(3).any(|w| w == [0, 0, 1] || w == [0, 0, 0]));
        assert_eq!(unescape_rbsp(&e), raw);
    }

    #[test]
    fn the_sei_carries_t_and_reads_back() {
        let data = sei_payload(4321.5 + 1.0 / 30.0, 1_761_134_400_000);
        let nal = sei_nal(&SEI_UUID, &data);
        assert_eq!(nal[0], 0x06);
        let msgs = parse_sei(&nal);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].0, SEI_UUID);
        let v: serde_json::Value = serde_json::from_slice(&msgs[0].1).unwrap();
        assert!((v["t"].as_f64().unwrap() - (4321.5 + 1.0 / 30.0)).abs() < 1e-6);
        assert_eq!(v["e"], 1_761_134_400_000u64);
    }

    #[test]
    fn a_long_sei_payload_uses_255_size_runs() {
        let data = vec![0u8; 600]; // zeros: escaping is exercised too
        let nal = sei_nal(&SEI_UUID, &data);
        let msgs = parse_sei(&nal);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].1, data);
    }

    fn au(key: bool) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, 0x09, 0xf0];
        if key {
            v.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0xAA]);
            v.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xBB]);
            v.extend_from_slice(&[0, 0, 1, 0x65, 0x88, 0x80]);
        } else {
            v.extend_from_slice(&[0, 0, 1, 0x41, 0x9a]);
        }
        v
    }

    /// Length-prefixed NAL units of an AVCC payload.
    fn units(b: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 4 <= b.len() {
            let n = u32::from_be_bytes(b[i..i + 4].try_into().unwrap()) as usize;
            out.push(b[i + 4..i + 4 + n].to_vec());
            i += 4 + n;
        }
        out
    }

    #[test]
    fn the_sei_goes_before_the_first_slice_and_delimiters_are_dropped() {
        let sei = sei_nal(&SEI_UUID, b"x");
        let u = to_avcc_with_sei(&au(true), &sei);
        let kinds: Vec<u8> = units(&u.avcc).iter().map(|n| n[0] & 0x1f).collect();
        assert_eq!(kinds, vec![7, 8, 6, 5]);
        assert_eq!(u.sps.as_deref(), Some(&[0x67, 0x64, 0x00, 0x28, 0xAA][..]));
        let u = to_avcc_with_sei(&au(false), &sei);
        let kinds: Vec<u8> = units(&u.avcc).iter().map(|n| n[0] & 0x1f).collect();
        assert_eq!(kinds, vec![6, 1]);
    }

    struct Tag {
        kind: u8,
        ts: u32,
        body: Vec<u8>,
    }

    fn tags(b: &[u8]) -> Vec<Tag> {
        assert_eq!(&b[..13], &FLV_HEADER);
        let mut out = Vec::new();
        let mut i = 13;
        while i < b.len() {
            let size = u32::from_be_bytes([0, b[i + 1], b[i + 2], b[i + 3]]) as usize;
            let ts = u32::from_be_bytes([b[i + 7], b[i + 4], b[i + 5], b[i + 6]]);
            out.push(Tag {
                kind: b[i],
                ts,
                body: b[i + 11..i + 11 + size].to_vec(),
            });
            let prev = u32::from_be_bytes(b[i + 11 + size..i + 15 + size].try_into().unwrap());
            assert_eq!(prev as usize, 11 + size);
            i += 15 + size;
        }
        out
    }

    fn frame(t: f64, key: bool) -> Frame {
        Frame {
            key,
            width: 1920,
            height: 1080,
            t,
            number: 0,
            au: au(key),
        }
    }

    #[test]
    fn the_timeline_starts_at_a_keyframe_and_stamps_scene_time() {
        let mut line = Timeline::default();
        let mut out = Vec::new();
        // A delta before any keyframe is not written.
        out.extend(line.frame(&frame(100.0, false), 7));
        assert!(out.is_empty());
        out.extend(line.frame(&frame(100.0 + 1.0 / 30.0, true), 7));
        out.extend(line.frame(&frame(100.0 + 2.0 / 30.0, false), 7));
        // A skipped slot is a gap in time, not a shift.
        out.extend(line.frame(&frame(100.0 + 5.0 / 30.0, false), 7));
        // Back in time: dropped.
        out.extend(line.frame(&frame(100.0 + 4.0 / 30.0, false), 7));
        let all = tags(&out);
        // The metadata first, naming the size.
        assert_eq!(all[0].kind, 18);
        assert!(all[0].body.windows(5).any(|w| w == b"width"));
        let t: Vec<&Tag> = all.iter().filter(|t| t.kind == 9).collect();
        let ts: Vec<u32> = t.iter().map(|t| t.ts).collect();
        assert_eq!(ts, vec![0, 0, 33, 133]);
        // The silent track: its configuration, then a frame every 1024 samples at 48 kHz up
        // to the newest video frame, each written before the video frame it does not pass.
        let audio: Vec<&Tag> = all.iter().filter(|t| t.kind == 8).collect();
        assert_eq!(audio[0].body, [0xAF, 0, 0x11, 0x88]);
        let ats: Vec<u32> = audio[1..].iter().map(|t| t.ts).collect();
        assert_eq!(ats, vec![0, 21, 42, 64, 85, 106, 128]);
        assert!(audio[1..]
            .iter()
            .all(|t| t.body == [0xAF, 1, 0x01, 0x18, 0x20, 0x07]));
        for (i, x) in all.iter().enumerate() {
            if x.kind == 8 && x.body[1] == 1 {
                let next_video = all[i..]
                    .iter()
                    .find(|v| v.kind == 9)
                    .expect("video after audio");
                assert!(
                    x.ts <= next_video.ts,
                    "audio at {} written after video at {}",
                    x.ts,
                    next_video.ts
                );
            }
        }
        // Sequence header, then units; every unit carries the SEI with its own t.
        assert_eq!(&t[0].body[..2], &[0x17, 0]);
        assert_eq!(
            t[0].body[5..],
            avc_config(&[0x67, 0x64, 0x00, 0x28, 0xAA], &[0x68, 0xBB])
        );
        for (tag, want) in t[1..].iter().zip([1.0, 2.0, 5.0]) {
            assert_eq!(tag.body[1], 1);
            let nals = units(&tag.body[5..]);
            let sei = nals.iter().find(|n| n[0] & 0x1f == 6).expect("an SEI");
            let msgs = parse_sei(sei);
            let v: serde_json::Value = serde_json::from_slice(&msgs[0].1).unwrap();
            assert!((v["t"].as_f64().unwrap() - (100.0 + want / 30.0)).abs() < 1e-6);
            assert_eq!(v["e"], 7);
        }
    }

    #[test]
    fn a_changed_configuration_sends_a_new_sequence_header() {
        let mut line = Timeline::default();
        let mut out = line.frame(&frame(1.0, true), 0);
        out.extend(line.frame(&frame(2.0, true), 0));
        let mut other = frame(3.0, true);
        other.au[13] = 0x1f; // a different level in the SPS
        out.extend(line.frame(&other, 0));
        let seq: Vec<u32> = tags(&out)
            .iter()
            .filter(|t| t.kind == 9 && t.body[1] == 0)
            .map(|t| t.ts)
            .collect();
        assert_eq!(seq, vec![0, 2000]);
    }

    /// The copy must not hold the stream back: with ffmpeg's default probing, frames fed to a
    /// copy in real time came out 0.9 s later at the median and 4.9 s at most; with these
    /// options, 0 ms. `-fflags nobuffer` drops what the probe read (the first GOP).
    #[test]
    fn the_copy_neither_probes_nor_buffers() {
        let a = ffmpeg_args("rtmps://ingest.example/app/k");
        let has = |pair: [&str; 2]| a.windows(2).any(|w| w[0] == pair[0] && w[1] == pair[1]);
        assert!(
            !a.iter().any(|x| x == "nobuffer"),
            "nobuffer drops the first GOP"
        );
        assert!(has(["-probesize", "32"]));
        assert!(has(["-analyzeduration", "0"]));
        assert!(has(["-c", "copy"]));
        assert_eq!(
            a.last().map(String::as_str),
            Some("rtmps://ingest.example/app/k")
        );
    }

    #[test]
    fn after_lost_frames_deltas_wait_for_a_keyframe() {
        let mut line = Timeline::default();
        assert!(!line.frame(&frame(1.0, true), 0).is_empty());
        line.lost_frames();
        assert!(line.frame(&frame(1.5, false), 0).is_empty());
        assert!(!line.frame(&frame(2.0, true), 0).is_empty());
        assert!(!line.frame(&frame(2.1, false), 0).is_empty());
    }

    /// The whole pusher with the real ffmpeg: libx264 frames published as viewers get them,
    /// pushed to a file instead of an ingest. ffprobe must find every frame at its scene time
    /// (milliseconds after the first), the silent audio beside them, and the SEI in each frame.
    #[cfg(feature = "ffmpeg-tests")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_pushed_stream_decodes_with_scene_time_stamps_and_sei() {
        use crate::fleet::aggregate::codec::{Codec, EncodeParams, Ffmpeg};
        use crate::fleet::aggregate::compose::Picture;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("push.flv");
        let shared = Shared::new();
        let cancel = CancellationToken::new();
        let pusher = tokio::spawn(run(
            shared.clone(),
            "ffmpeg".into(),
            out.to_string_lossy().into_owned(),
            cancel.clone(),
        ));
        // The pusher subscribes once its ffmpeg has started.
        tokio::time::timeout(Duration::from_secs(10), async {
            while shared.out.receiver_count() == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the pusher subscribes");
        let ff = Ffmpeg {
            bin: "ffmpeg".into(),
        };
        let (w, h) = (320u32, 180u32);
        let (tx, mut rx) = ff
            .encoder(
                w,
                h,
                30,
                &EncodeParams {
                    keyint: 30,
                    ..Default::default()
                },
            )
            .unwrap();
        // 60 slots from t = 500, skipping slots 40 to 44.
        let ks: Vec<i64> = (0..60).filter(|k| !(40..45).contains(k)).collect();
        for &k in &ks {
            let mut p = Picture::filled(w, h, [16, 128, 128]);
            p.data[(k as usize * 7) % (w * h) as usize] = 235;
            let t = 500.0 + k as f64 / 30.0;
            tx.send((t, p)).await.unwrap();
            let e = rx.recv().await.unwrap();
            let bytes = Frame {
                key: e.key,
                width: w as u16,
                height: h as u16,
                t: e.t,
                number: k as u32,
                au: e.au,
            }
            .encode();
            shared.publish(Out::Frame { key: e.key, bytes });
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        cancel.cancel();
        pusher.await.unwrap();
        let probe = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "packet=codec_type,pts_time",
                "-of",
                "json",
            ])
            .arg(&out)
            .output()
            .unwrap();
        assert!(
            probe.status.success(),
            "{}",
            String::from_utf8_lossy(&probe.stderr)
        );
        let v: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
        let packets = v["packets"].as_array().unwrap();
        let video: Vec<f64> = packets
            .iter()
            .filter(|p| p["codec_type"] == "video")
            .map(|p| p["pts_time"].as_str().unwrap().parse().unwrap())
            .collect();
        let audio = packets
            .iter()
            .filter(|p| p["codec_type"] == "audio")
            .count();
        assert!(
            (90..=95).contains(&audio),
            "{audio} silent audio frames for 1.97 s"
        );
        let want: Vec<f64> = ks
            .iter()
            .map(|k| (*k as f64 / 30.0 * 1000.0).round() / 1000.0)
            .collect();
        assert_eq!(video.len(), want.len(), "every frame pushed");
        for (got, want) in video.iter().zip(&want) {
            assert!(
                (got - want).abs() < 0.0015,
                "pts {got} for scene offset {want}"
            );
        }
        // The SEI survives the copy: read the file's video tags back.
        let bytes = std::fs::read(&out).unwrap();
        let mut seis = 0;
        let mut i = 13;
        while i + 11 <= bytes.len() {
            let size = u32::from_be_bytes([0, bytes[i + 1], bytes[i + 2], bytes[i + 3]]) as usize;
            if bytes[i] == 9 && bytes[i + 12] == 1 {
                let body = &bytes[i + 16..i + 11 + size];
                let mut j = 0;
                while j + 4 <= body.len() {
                    let n = u32::from_be_bytes(body[j..j + 4].try_into().unwrap()) as usize;
                    let nal = &body[j + 4..j + 4 + n];
                    if nal[0] & 0x1f == 6 && parse_sei(nal).iter().any(|(u, _)| *u == SEI_UUID) {
                        seis += 1;
                    }
                    j += 4 + n;
                }
            }
            i += 15 + size;
        }
        assert_eq!(seis, ks.len(), "one SEI per frame");
    }

    /// A child that takes nothing (a stalled ingest: ffmpeg alive, its stdin full) must
    /// neither hold up cancellation nor keep the push from starting again.
    fn stalled_ffmpeg(dir: &std::path::Path) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("ffmpeg");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\necho started >> {}/starts\nexec sleep 600\n",
                dir.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    /// Frames big enough that a few fill the pipe to a child that does not read.
    fn publish_big_frames(shared: &Shared, n: usize) {
        for i in 0..n {
            let mut f = frame(10.0 + i as f64 / 30.0, true);
            f.au.extend_from_slice(&[0, 0, 0, 1, 0x0c]);
            f.au.extend(std::iter::repeat_n(0xff, 256 * 1024));
            shared.publish(Out::Frame {
                key: true,
                bytes: f.encode(),
            });
        }
    }

    async fn subscribed(shared: &Shared) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while shared.out.receiver_count() == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the pusher subscribes");
    }

    #[tokio::test]
    async fn a_stalled_ingest_does_not_hold_up_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Shared::new();
        let cancel = CancellationToken::new();
        let pusher = tokio::spawn(run(
            shared.clone(),
            stalled_ffmpeg(dir.path()),
            "rtmps://ingest.example/app/k".into(),
            cancel.clone(),
        ));
        subscribed(&shared).await;
        publish_big_frames(&shared, 8);
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), pusher)
            .await
            .expect("the pusher stops within 2 s of its cancellation")
            .unwrap();
    }

    #[tokio::test]
    async fn a_stalled_ingest_is_started_again() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Shared::new();
        let cancel = CancellationToken::new();
        let pusher = tokio::spawn(run(
            shared.clone(),
            stalled_ffmpeg(dir.path()),
            "rtmps://ingest.example/app/k".into(),
            cancel.clone(),
        ));
        subscribed(&shared).await;
        publish_big_frames(&shared, 8);
        let starts = dir.path().join("starts");
        let deadline = std::time::Instant::now() + WRITE_STALL + Duration::from_secs(5);
        loop {
            let n = std::fs::read_to_string(&starts)
                .unwrap_or_default()
                .lines()
                .count();
            if n >= 2 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the stalled child was not replaced ({n} start)"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        cancel.cancel();
        pusher.await.unwrap();
    }
}
