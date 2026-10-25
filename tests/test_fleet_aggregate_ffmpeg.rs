//! The aggregator's ffmpeg codec: libx264 out, the H.264 decoder back in. Needs the ffmpeg
//! CLI with libx264, so it is compiled only with `--features ffmpeg-tests`:
//!
//!     cargo test --features ffmpeg-tests --test test_fleet_aggregate_ffmpeg
#![cfg(feature = "ffmpeg-tests")]

use fcvm::fleet::aggregate::codec::{Codec, EncodeParams, Ffmpeg};
use fcvm::fleet::aggregate::compose::Picture;
use fcvm::fleet::aggregate::frame::{codec_string, is_keyframe};
use std::time::{Duration, Instant};

/// A moving gradient, so every frame differs.
fn picture(w: u32, h: u32, k: u32) -> Picture {
    let mut p = Picture::filled(w, h, [16, 128, 128]);
    let plane = (w * h) as usize;
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) as usize;
            p.data[i] = 16 + ((x + y + 2 * k) % 200) as u8;
            p.data[plane + i] = 128 + ((x * 2 + k) % 40) as u8;
            p.data[2 * plane + i] = 108 + ((y * 2) % 40) as u8;
        }
    }
    p
}

fn psnr_y(a: &Picture, b: &Picture) -> f64 {
    let n = (a.width * a.height) as usize;
    let mse: f64 = a.data[..n]
        .iter()
        .zip(&b.data[..n])
        .map(|(x, y)| (*x as f64 - *y as f64).powi(2))
        .sum::<f64>()
        / n as f64;
    10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn x264_out_and_h264_in_keep_every_frame_its_t() {
    let ff = Ffmpeg {
        bin: "ffmpeg".into(),
    };
    let (w, h) = (320, 180);
    let (etx, mut erx) = ff
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
    let mut sent = Vec::new();
    let mut lat = Vec::new();
    let mut aus = Vec::new();
    for k in 0..90u32 {
        let t = 100.0 + k as f64 / 30.0;
        let at = Instant::now();
        etx.send((t, picture(w, h, k))).await.unwrap();
        sent.push(t);
        let e = tokio::time::timeout(Duration::from_secs(10), erx.recv())
            .await
            .unwrap()
            .unwrap();
        lat.push(at.elapsed());
        assert_eq!(e.t, t, "encoded unit {k} carries another frame's t");
        assert_eq!(e.key, k % 30 == 0, "frame {k}");
        assert_eq!(e.key, is_keyframe(&e.au), "frame {k}");
        aus.push(e);
    }
    assert!(
        codec_string(&aus[0].au).unwrap().starts_with("avc1.64"),
        "High profile"
    );
    lat.sort();
    eprintln!("encode latency p50 {:?} max {:?}", lat[45], lat[89]);
    assert!(
        lat[45] < Duration::from_millis(100),
        "one frame in, one unit out, at once: {:?}",
        lat[45]
    );

    // The ffmpeg CLI hands out each decoded picture when the next unit arrives (one frame
    // late; see codec.rs), so feed everything, close, and read every picture back in order.
    let (dtx, mut drx) = ff.decoder(w, h).unwrap();
    let feeder = tokio::spawn({
        let aus: Vec<(f64, Vec<u8>)> = aus.iter().map(|e| (e.t, e.au.clone())).collect();
        async move {
            for a in aus {
                dtx.send(a).await.unwrap();
            }
        }
    });
    for (k, e) in aus.iter().enumerate() {
        let (t, pic) = tokio::time::timeout(Duration::from_secs(10), drx.recv())
            .await
            .unwrap_or_else(|_| panic!("picture {k} within 10 s"))
            .unwrap();
        assert_eq!(t, e.t, "decoded picture {k} carries another frame's t");
        let p = psnr_y(&pic, &picture(w, h, k as u32));
        assert!(p > 35.0, "frame {k}: PSNR {p:.1} dB");
    }
    feeder.await.unwrap();
}

/// The poster's JPEG: ffmpeg encodes the composed picture, and decoding it back gives the
/// colours that went in.
#[tokio::test]
async fn the_poster_is_a_jpeg_of_the_picture() {
    use fcvm::fleet::coord::poster::StillEncoder;
    let mut p = Picture::filled(64, 32, [63, 102, 240]); // red
    p.fill_rect(32, 0, 32, 32, [32, 240, 118]); // blue
    let enc = StillEncoder::Jpeg {
        ffmpeg: "ffmpeg".into(),
    };
    let (ct, jpeg) = enc.encode(&p).await.unwrap();
    assert_eq!(ct, "image/jpeg");
    assert!(jpeg.starts_with(&[0xff, 0xd8]));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.jpg");
    std::fs::write(&path, &jpeg).unwrap();
    let out = std::process::Command::new("ffmpeg")
        .args(["-loglevel", "error", "-i"])
        .arg(&path)
        .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let rgb = out.stdout;
    assert_eq!(rgb.len(), 64 * 32 * 3);
    let px = |x: usize, y: usize| &rgb[(y * 64 + x) * 3..(y * 64 + x) * 3 + 3];
    let near = |got: &[u8], want: [u8; 3]| got.iter().zip(want).all(|(g, w)| g.abs_diff(w) <= 12);
    assert!(near(px(8, 16), [255, 0, 0]), "{:?}", px(8, 16));
    assert!(near(px(56, 16), [0, 0, 255]), "{:?}", px(56, 16));
}
