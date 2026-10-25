//! Decoding tiles and encoding the region. [`Ffmpeg`] runs the `ffmpeg` CLI (H.264 decode;
//! libx264 encode, no B-frames, zero-latency tuning) as child processes, so nothing GPL is
//! linked into fcvm. [`Raw`] passes uncompressed pictures through, for tests.
//!
//! Both directions keep frames in order and pair each output with its input's `t` through
//! a FIFO: with no B-frames and no lookahead, every input yields exactly one output, in
//! order.

use super::compose::Picture;
use anyhow::{bail, Context, Result};
use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

/// One encoded access unit of the region, Annex-B.
#[derive(Debug, Clone)]
pub struct Encoded {
    pub t: f64,
    pub key: bool,
    pub au: Vec<u8>,
}

pub type DecoderIo = (mpsc::Sender<(f64, Vec<u8>)>, mpsc::Receiver<(f64, Picture)>);
pub type EncoderIo = (mpsc::Sender<(f64, Picture)>, mpsc::Receiver<Encoded>);

#[derive(Debug, Clone)]
pub struct EncodeParams {
    pub preset: String,
    pub crf: u32,
    pub maxrate_kbps: u32,
    pub keyint: u32,
}

impl Default for EncodeParams {
    fn default() -> Self {
        EncodeParams {
            preset: "veryfast".into(),
            crf: 20,
            maxrate_kbps: 20_000,
            keyint: 60,
        }
    }
}

pub trait Codec: Send + Sync {
    /// A decoder of `width` x `height` H.264: feed (t, access unit), read (t, picture).
    /// Dropping the sender ends it.
    fn decoder(&self, width: u32, height: u32) -> Result<DecoderIo>;
    /// An encoder of `width` x `height` pictures at `fps`.
    fn encoder(&self, width: u32, height: u32, fps: u32, p: &EncodeParams) -> Result<EncoderIo>;
    /// The codec string a header names before the first keyframe is seen.
    fn name(&self) -> &'static str;
}

/// Uncompressed pictures as "access units": the decoder passes them through, the encoder
/// emits the picture bytes, marking every `keyint`-th as a keyframe.
pub struct Raw;

impl Codec for Raw {
    fn decoder(&self, width: u32, height: u32) -> Result<DecoderIo> {
        Ok(raw_decoder(width, height, 64))
    }

    fn encoder(&self, width: u32, height: u32, _fps: u32, p: &EncodeParams) -> Result<EncoderIo> {
        Ok(raw_encoder(width, height, p.keyint, 64))
    }

    fn name(&self) -> &'static str {
        "raw"
    }
}

/// [`Raw`]'s decoder, with channels of `cap` messages.
fn raw_decoder(width: u32, height: u32, cap: usize) -> DecoderIo {
    let (in_tx, mut in_rx) = mpsc::channel::<(f64, Vec<u8>)>(cap);
    let (out_tx, out_rx) = mpsc::channel(cap);
    tokio::spawn(async move {
        while let Some((t, au)) = in_rx.recv().await {
            if au.len() != Picture::size(width, height) {
                tracing::warn!(len = au.len(), "raw tile picture of the wrong size");
                continue;
            }
            let pic = Picture {
                width,
                height,
                data: au,
            };
            if out_tx.send((t, pic)).await.is_err() {
                return;
            }
        }
    });
    (in_tx, out_rx)
}

/// [`Raw`]'s encoder, with channels of `cap` messages.
fn raw_encoder(width: u32, height: u32, keyint: u32, cap: usize) -> EncoderIo {
    let (in_tx, mut in_rx) = mpsc::channel::<(f64, Picture)>(cap);
    let (out_tx, out_rx) = mpsc::channel(cap);
    let keyint = keyint.max(1) as u64;
    tokio::spawn(async move {
        let mut n = 0u64;
        while let Some((t, pic)) = in_rx.recv().await {
            if pic.width != width || pic.height != height {
                continue;
            }
            let e = Encoded {
                t,
                key: n.is_multiple_of(keyint),
                au: pic.data,
            };
            n += 1;
            if out_tx.send(e).await.is_err() {
                return;
            }
        }
    });
    (in_tx, out_rx)
}

/// [`Raw`] with channels of one message (the least room a real codec's pipes can leave),
/// whose `encoder` calls numbered in `refuse` (from 0) fail to start.
#[cfg(test)]
pub(crate) struct Narrow {
    pub calls: std::sync::atomic::AtomicUsize,
    pub refuse: Vec<usize>,
}

#[cfg(test)]
impl Narrow {
    pub fn new(refuse: Vec<usize>) -> Self {
        Narrow {
            calls: Default::default(),
            refuse,
        }
    }
}

#[cfg(test)]
impl Codec for Narrow {
    fn decoder(&self, width: u32, height: u32) -> Result<DecoderIo> {
        Ok(raw_decoder(width, height, 1))
    }

    fn encoder(&self, width: u32, height: u32, _fps: u32, p: &EncodeParams) -> Result<EncoderIo> {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.refuse.contains(&n) {
            bail!("encoder {n} refused to start");
        }
        Ok(raw_encoder(width, height, p.keyint, 1))
    }

    fn name(&self) -> &'static str {
        "raw"
    }
}

/// The `ffmpeg` CLI. `bin` is its path.
pub struct Ffmpeg {
    pub bin: String,
}

/// An access unit delimiter, written after each access unit so ffmpeg's H.264 parser ends
/// the unit without waiting for the next one's first bytes. The ffmpeg CLI (6.1) still
/// hands out each decoded picture only once the next unit arrives: decoding lags one frame
/// (33 ms at 30 fps), with every low-delay flag tried; `-fflags nobuffer` makes it output
/// nothing at all.
const AUD: [u8; 6] = [0, 0, 0, 1, 0x09, 0xf0];

fn spawn(bin: &str, args: &[String]) -> Result<tokio::process::Child> {
    tokio::process::Command::new(bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {bin}"))
}

impl Codec for Ffmpeg {
    fn decoder(&self, width: u32, height: u32) -> Result<DecoderIo> {
        let args: Vec<String> = [
            "-hide_banner",
            "-loglevel",
            "error",
            "-flags",
            "low_delay",
            "-probesize",
            "32",
            "-analyzeduration",
            "0",
            "-threads",
            "1",
            "-f",
            "h264",
            "-i",
            "pipe:0",
            "-fps_mode",
            "passthrough",
            "-pix_fmt",
            "yuv444p",
            "-f",
            "rawvideo",
            "-flush_packets",
            "1",
            "pipe:1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let mut child = spawn(&self.bin, &args)?;
        let mut stdin = child.stdin.take().context("decoder stdin")?;
        let mut stdout = child.stdout.take().context("decoder stdout")?;
        let ts: Arc<Mutex<VecDeque<f64>>> = Arc::default();
        let (in_tx, mut in_rx) = mpsc::channel::<(f64, Vec<u8>)>(64);
        let (out_tx, out_rx) = mpsc::channel(64);
        let ts_w = ts.clone();
        tokio::spawn(async move {
            while let Some((t, au)) = in_rx.recv().await {
                ts_w.lock().unwrap().push_back(t);
                if stdin.write_all(&au).await.is_err() || stdin.write_all(&AUD).await.is_err() {
                    return;
                }
                let _ = stdin.flush().await;
            }
        });
        tokio::spawn(async move {
            let _child = child;
            let size = Picture::size(width, height);
            loop {
                let mut buf = vec![0u8; size];
                if stdout.read_exact(&mut buf).await.is_err() {
                    return;
                }
                let Some(t) = ts.lock().unwrap().pop_front() else {
                    tracing::warn!("decoder produced a picture with no input left to pair");
                    return;
                };
                let pic = Picture {
                    width,
                    height,
                    data: buf,
                };
                if out_tx.send((t, pic)).await.is_err() {
                    return;
                }
            }
        });
        Ok((in_tx, out_rx))
    }

    fn encoder(&self, width: u32, height: u32, fps: u32, p: &EncodeParams) -> Result<EncoderIo> {
        let args: Vec<String> = vec![
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            "-f".into(),
            "rawvideo".into(),
            "-pix_fmt".into(),
            "yuv444p".into(),
            "-s".into(),
            format!("{width}x{height}"),
            "-framerate".into(),
            fps.to_string(),
            "-i".into(),
            "pipe:0".into(),
            "-fps_mode".into(),
            "passthrough".into(),
            "-c:v".into(),
            "libx264".into(),
            "-preset".into(),
            p.preset.clone(),
            "-tune".into(),
            "zerolatency".into(),
            "-profile:v".into(),
            "high".into(),
            "-pix_fmt".into(),
            "yuv420p".into(),
            "-bf".into(),
            "0".into(),
            "-g".into(),
            p.keyint.to_string(),
            "-keyint_min".into(),
            p.keyint.to_string(),
            "-sc_threshold".into(),
            "0".into(),
            "-crf".into(),
            p.crf.to_string(),
            "-maxrate".into(),
            format!("{}k", p.maxrate_kbps),
            "-bufsize".into(),
            format!("{}k", p.maxrate_kbps / 2),
            "-x264-params".into(),
            "repeat-headers=1".into(),
            "-color_range".into(),
            "tv".into(),
            "-colorspace".into(),
            "bt709".into(),
            "-color_primaries".into(),
            "bt709".into(),
            "-color_trc".into(),
            "bt709".into(),
            "-flush_packets".into(),
            "1".into(),
            "-f".into(),
            "flv".into(),
            "pipe:1".into(),
        ];
        let mut child = spawn(&self.bin, &args)?;
        let mut stdin = child.stdin.take().context("encoder stdin")?;
        let stdout = child.stdout.take().context("encoder stdout")?;
        let ts: Arc<Mutex<VecDeque<f64>>> = Arc::default();
        let (in_tx, mut in_rx) = mpsc::channel::<(f64, Picture)>(8);
        let (out_tx, out_rx) = mpsc::channel(64);
        let ts_w = ts.clone();
        tokio::spawn(async move {
            while let Some((t, pic)) = in_rx.recv().await {
                if pic.width != width || pic.height != height {
                    continue;
                }
                ts_w.lock().unwrap().push_back(t);
                if stdin.write_all(&pic.data).await.is_err() {
                    return;
                }
            }
        });
        tokio::spawn(async move {
            let _child = child;
            let mut flv = Flv::new(stdout);
            loop {
                match flv.next_video().await {
                    Ok(Some((key, au))) => {
                        let Some(t) = ts.lock().unwrap().pop_front() else {
                            tracing::warn!("encoder produced a unit with no input left to pair");
                            return;
                        };
                        if out_tx.send(Encoded { t, key, au }).await.is_err() {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), "reading the encoder failed");
                        return;
                    }
                }
            }
        });
        Ok((in_tx, out_rx))
    }

    fn name(&self) -> &'static str {
        "avc1.640028"
    }
}

/// Reads H.264 access units out of an FLV stream, which frames each one with its length
/// (a raw H.264 stream would have to wait for the next unit's start to end this one).
pub struct Flv<R> {
    r: R,
    started: bool,
    sps_pps: Vec<u8>,
}

impl<R: tokio::io::AsyncRead + Unpin> Flv<R> {
    pub fn new(r: R) -> Self {
        Flv {
            r,
            started: false,
            sps_pps: Vec::new(),
        }
    }

    /// The next video access unit as (keyframe, Annex-B bytes), or `None` at the end.
    pub async fn next_video(&mut self) -> Result<Option<(bool, Vec<u8>)>> {
        if !self.started {
            let mut head = [0u8; 13];
            if self.r.read_exact(&mut head).await.is_err() {
                return Ok(None);
            }
            if &head[..3] != b"FLV" {
                bail!("not an FLV stream");
            }
            self.started = true;
        }
        loop {
            let mut tag = [0u8; 11];
            if self.r.read_exact(&mut tag).await.is_err() {
                return Ok(None);
            }
            let size = u32::from_be_bytes([0, tag[1], tag[2], tag[3]]) as usize;
            let mut data = vec![0u8; size + 4];
            self.r.read_exact(&mut data).await?;
            data.truncate(size);
            if tag[0] != 9 || data.len() < 5 {
                continue;
            }
            let key = data[0] >> 4 == 1;
            match data[1] {
                0 => self.sps_pps = avcc_config_to_annexb(&data[5..])?,
                1 => {
                    let mut au = avcc_to_annexb(&data[5..])?;
                    let has_sps =
                        super::frame::nal_units(&au).any(|n| !n.is_empty() && n[0] & 0x1f == 7);
                    if key && !has_sps {
                        let mut with = self.sps_pps.clone();
                        with.extend_from_slice(&au);
                        au = with;
                    }
                    return Ok(Some((key, au)));
                }
                _ => {}
            }
        }
    }
}

/// Length-prefixed (4-byte) NAL units to Annex-B.
pub fn avcc_to_annexb(b: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(b.len() + 16);
    let mut i = 0;
    while i + 4 <= b.len() {
        let n = u32::from_be_bytes(b[i..i + 4].try_into().unwrap()) as usize;
        i += 4;
        if i + n > b.len() {
            bail!("NAL unit runs past its packet");
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&b[i..i + n]);
        i += n;
    }
    Ok(out)
}

/// An AVCDecoderConfigurationRecord's SPS and PPS, Annex-B.
pub fn avcc_config_to_annexb(b: &[u8]) -> Result<Vec<u8>> {
    if b.len() < 7 {
        bail!("short avcC");
    }
    let mut out = Vec::new();
    let mut i = 5;
    let nsps = b[i] & 0x1f;
    i += 1;
    for _ in 0..nsps {
        let n = u16::from_be_bytes([b[i], b[i + 1]]) as usize;
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(b.get(i + 2..i + 2 + n).context("short avcC SPS")?);
        i += 2 + n;
    }
    let npps = *b.get(i).context("avcC without PPS count")?;
    i += 1;
    for _ in 0..npps {
        let n = u16::from_be_bytes([b[i], b[i + 1]]) as usize;
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(b.get(i + 2..i + 2 + n).context("short avcC PPS")?);
        i += 2 + n;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(kind: u8, data: &[u8]) -> Vec<u8> {
        let mut t = vec![kind];
        t.extend_from_slice(&(data.len() as u32).to_be_bytes()[1..]);
        t.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0]);
        t.extend_from_slice(data);
        t.extend_from_slice(&((data.len() + 11) as u32).to_be_bytes());
        t
    }

    #[tokio::test]
    async fn flv_units_come_out_annex_b_with_headers_on_keyframes() {
        let mut s = b"FLV\x01\x01\x00\x00\x00\x09\x00\x00\x00\x00".to_vec();
        // avcC: version, profile, compat, level, length size, 1 SPS, then 1 PPS.
        let avcc = [
            1, 0x64, 0, 0x28, 0xff, 0xe1, 0, 4, 0x67, 0x64, 0x00, 0x01, 1, 0, 2, 0x68, 0xee,
        ];
        let mut seq = vec![0x17, 0, 0, 0, 0];
        seq.extend_from_slice(&avcc);
        s.extend(tag(18, b"meta"));
        s.extend(tag(9, &seq));
        let mut idr = vec![0x17, 1, 0, 0, 0];
        idr.extend_from_slice(&[0, 0, 0, 2, 0x65, 0x88]);
        s.extend(tag(9, &idr));
        let mut p = vec![0x27, 1, 0, 0, 0];
        p.extend_from_slice(&[0, 0, 0, 2, 0x41, 0x9a, 0, 0, 0, 1, 0x06]);
        s.extend(tag(9, &p));
        let mut flv = Flv::new(&s[..]);
        let (k, au) = flv.next_video().await.unwrap().unwrap();
        assert!(k);
        assert_eq!(
            au,
            vec![
                0, 0, 0, 1, 0x67, 0x64, 0x00, 0x01, 0, 0, 0, 1, 0x68, 0xee, 0, 0, 0, 1, 0x65, 0x88
            ]
        );
        assert_eq!(
            super::super::frame::codec_string(&au).as_deref(),
            Some("avc1.640001")
        );
        let (k, au) = flv.next_video().await.unwrap().unwrap();
        assert!(!k);
        assert_eq!(au, vec![0, 0, 0, 1, 0x41, 0x9a, 0, 0, 0, 1, 0x06]);
        assert!(flv.next_video().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn raw_codec_pairs_each_output_with_its_t() {
        let (tx, mut rx) = Raw.decoder(2, 2).unwrap();
        tx.send((1.5, vec![7; 12])).await.unwrap();
        tx.send((1.6, vec![1; 5])).await.unwrap(); // wrong size: dropped
        tx.send((1.7, vec![8; 12])).await.unwrap();
        assert_eq!(rx.recv().await.unwrap().0, 1.5);
        assert_eq!(rx.recv().await.unwrap().0, 1.7);
        let (tx, mut rx) = Raw
            .encoder(
                2,
                2,
                30,
                &EncodeParams {
                    keyint: 2,
                    ..Default::default()
                },
            )
            .unwrap();
        for i in 0..3 {
            tx.send((i as f64, Picture::filled(2, 2, [i, 0, 0])))
                .await
                .unwrap();
        }
        let keys: Vec<bool> = [rx.recv().await, rx.recv().await, rx.recv().await]
            .into_iter()
            .map(|e| e.unwrap().key)
            .collect();
        assert_eq!(keys, vec![true, false, true]);
    }
}
