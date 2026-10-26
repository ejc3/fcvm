//! The aggregator end to end with the raw codec (uncompressed pictures as access units, so
//! no ffmpeg is needed): fake renderers stream tiles with crop margins, the aggregator
//! joins them at one `t`, and a viewer checks every pixel of the region.

use axum::extract::ws::{Message as AxMessage, WebSocket};
use axum::extract::{Query, State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use fcvm::fleet::aggregate::codec::{EncodeParams, Raw};
use fcvm::fleet::aggregate::compose::Picture;
use fcvm::fleet::aggregate::frame::{Crop, Frame, Header};
use fcvm::fleet::aggregate::{router, run, Layout, Options, Shared, TileSpec};
use fleet_core::tiling::Rect;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

const FPS: u32 = 30;
const MARGIN: u32 = 4;
const MARGIN_Y: u8 = 250;

#[derive(Clone)]
struct FakeTile {
    header: Header,
    /// U value identifying the tile.
    id_u: u8,
    epoch: u64,
    queries: Arc<Mutex<Vec<HashMap<String, String>>>>,
    /// The first slot it sends (a renderer that comes up late).
    from_k: i64,
    /// From this slot on it draws another rectangle: a new header (a new size) mid-stream,
    /// as a renderer does at a re-tile's frame.
    switch: Option<(i64, Header)>,
    /// Milliseconds each frame goes out after its slot's wall time (a slower renderer).
    lag_ms: f64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// The picture a fake tile sends for slot `k`: inner pixels Y = 20 + k % 200, U = its id;
/// margin pixels Y = 250.
fn tile_picture(h: &Header, id_u: u8, k: i64) -> Picture {
    let mut p = Picture::filled(h.width, h.height, [MARGIN_Y, id_u, 128]);
    let iw = h.width - h.crop.left - h.crop.right;
    let ih = h.height - h.crop.top - h.crop.bottom;
    p.fill_rect(
        h.crop.left,
        h.crop.top,
        iw,
        ih,
        [20 + (k % 200) as u8, id_u, 128],
    );
    p
}

async fn tile_ws(
    ws: WebSocketUpgrade,
    Query(q): Query<HashMap<String, String>>,
    State(t): State<FakeTile>,
) -> impl IntoResponse {
    t.queries.lock().unwrap().push(q);
    ws.on_upgrade(move |s| serve_tile(s, t))
}

async fn serve_tile(mut s: WebSocket, mut t: FakeTile) {
    if s.send(AxMessage::Text(
        serde_json::to_string(&t.header).unwrap().into(),
    ))
    .await
    .is_err()
    {
        return;
    }
    let mut k = (((now_ms() - t.epoch) as f64 * FPS as f64 / 1000.0).ceil() as i64).max(t.from_k);
    loop {
        let due = t.epoch as f64 + k as f64 * 1000.0 / FPS as f64 + t.lag_ms;
        let wait = due - now_ms() as f64;
        if wait > 0.0 {
            tokio::time::sleep(Duration::from_micros((wait * 1000.0) as u64)).await;
        }
        if let Some((_, h)) = t.switch.clone().filter(|(at, _)| k >= *at) {
            t.header = h;
            t.switch = None;
            let text = serde_json::to_string(&t.header).unwrap();
            if s.send(AxMessage::Text(text.into())).await.is_err() {
                return;
            }
        }
        let f = Frame {
            key: true,
            width: t.header.width as u16,
            height: t.header.height as u16,
            t: k as f64 / FPS as f64,
            number: k as u32,
            au: tile_picture(&t.header, t.id_u, k).data,
        };
        if s.send(AxMessage::Binary(f.encode().into())).await.is_err() {
            return;
        }
        k += 1;
    }
}

async fn start_tile(t: FakeTile) -> String {
    let app = Router::new().route("/frames", get(tile_ws)).with_state(t);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("ws://{a}/frames")
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn next_msg(ws: &mut Ws) -> Message {
    tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .expect("a message within 10 s")
        .unwrap()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tiles_are_joined_at_one_t_without_their_margins() {
    // The fcvm half of a 128 x 72 image (16:9): a 64 x 72 region of four 32 x 36 tiles.
    let epoch = now_ms() - 10_000;
    let queries = Arc::new(Mutex::new(Vec::new()));
    let tile = |x0: u32, y0: u32, id_u: u8| {
        // Margins on the sides that face another tile inside the image.
        let crop = Crop {
            left: if x0 > 0 { MARGIN } else { 0 },
            top: if y0 > 0 { MARGIN } else { 0 },
            right: MARGIN,
            bottom: if y0 == 0 { MARGIN } else { 0 },
        };
        FakeTile {
            header: Header {
                codec: "raw".into(),
                width: 32 + crop.left + crop.right,
                height: 36 + crop.top + crop.bottom,
                full_width: 128,
                full_height: 72,
                x0,
                y0,
                fps: FPS,
                crop,
            },
            id_u,
            epoch,
            queries: queries.clone(),
            from_k: i64::MIN,
            switch: None,
            lag_ms: 0.0,
        }
    };
    let urls = [
        Some(start_tile(tile(0, 0, 60)).await),
        Some(start_tile(tile(32, 0, 90)).await),
        // A dark tile (nothing to stream) and one whose renderer is down.
        None,
        Some("ws://127.0.0.1:9/frames".to_string()),
    ];
    let rects = [
        Rect {
            x: 0.0,
            y: 0.0,
            w: 0.25,
            h: 0.5,
        },
        Rect {
            x: 0.25,
            y: 0.0,
            w: 0.25,
            h: 0.5,
        },
        Rect {
            x: 0.0,
            y: 0.5,
            w: 0.25,
            h: 0.5,
        },
        Rect {
            x: 0.25,
            y: 0.5,
            w: 0.25,
            h: 0.5,
        },
    ];
    let shared = Shared::new();
    shared.set_layout(Layout {
        epoch_unix_ms: epoch,
        image_aspect: 16.0 / 9.0,
        region: Rect {
            x: 0.0,
            y: 0.0,
            w: 0.5,
            h: 1.0,
        },
        tiles: (0..4)
            .map(|i| TileSpec {
                id: format!("slot-{}", i + 1),
                rect: rects[i],
                url: urls[i].clone(),
            })
            .collect(),
        generation: 1,
        from_frame: 0,
    });
    let cancel = CancellationToken::new();
    let opts = Options {
        image_height: 72,
        encode: EncodeParams {
            keyint: 15,
            ..Default::default()
        },
        ..Options::default()
    };
    tokio::spawn(run(shared.clone(), Arc::new(Raw), opts, cancel.clone()));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let s2 = shared.clone();
    tokio::spawn(async move { axum::serve(l, router(s2)).await.unwrap() });

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/frames"))
        .await
        .unwrap();
    let h: serde_json::Value = match next_msg(&mut ws).await {
        Message::Text(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("first message must be the header, got {other:?}"),
    };
    assert_eq!(
        (
            &h["width"],
            &h["height"],
            &h["full_width"],
            &h["full_height"],
            &h["x0"],
            &h["y0"]
        ),
        (
            &64.into(),
            &72.into(),
            &128.into(),
            &72.into(),
            &0.into(),
            &0.into()
        )
    );
    assert_eq!(
        h["crop"],
        serde_json::json!({"left": 0, "top": 0, "right": 0, "bottom": 0})
    );
    assert_eq!(h["aggregate"], true);

    let mut last_t = f64::MIN;
    let mut checked = 0;
    let mut all_exact = 0;
    while checked < 60 {
        let Message::Binary(b) = next_msg(&mut ws).await else {
            continue;
        };
        let f = Frame::parse(&b).unwrap();
        assert!(f.t > last_t, "t went back: {} after {}", f.t, last_t);
        let k = (f.t * FPS as f64).round() as i64;
        assert!(
            (f.t * FPS as f64 - k as f64).abs() < 1e-6,
            "t {} is off the 1/30 grid",
            f.t
        );
        last_t = f.t;
        let pic = Picture {
            width: 64,
            height: 72,
            data: f.au,
        };
        // Until both renderers are streaming the region is black where they go.
        if checked == 0 && (pic.get(0, 0)[1] != 60 || pic.get(32, 0)[1] != 90) {
            continue;
        }
        let want_y = 20 + (k % 200) as u8;
        let mut exact = true;
        for y in 0..72 {
            for x in 0..64 {
                let p = pic.get(x, y);
                assert_ne!(
                    p[0], MARGIN_Y,
                    "a crop margin pixel reached ({x},{y}) at t {}",
                    f.t
                );
                match (x < 32, y < 36) {
                    (true, true) => assert_eq!(p[1], 60, "tile 1 at ({x},{y})"),
                    (false, true) => assert_eq!(p[1], 90, "tile 2 at ({x},{y})"),
                    _ => assert_eq!(
                        p,
                        [16, 128, 128],
                        "a dark or down tile must be black at ({x},{y})"
                    ),
                }
                if y < 36 && p[0] != want_y {
                    exact = false;
                }
            }
        }
        all_exact += exact as u32;
        checked += 1;
    }
    // Both streaming tiles show the frame of the output's own t (a late tile may show its
    // previous frame, held, but that is rare on loopback).
    assert!(
        all_exact >= 55,
        "only {all_exact} of 60 frames had every tile at the frame's t"
    );

    // The aggregator asked each renderer for its tile's height in this image, at q=hi.
    for q in queries.lock().unwrap().iter() {
        assert_eq!(q.get("h").map(String::as_str), Some("36"));
        assert_eq!(q.get("q").map(String::as_str), Some("hi"));
    }

    // A viewer joining later starts with the header and a keyframe.
    let (mut late, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/frames"))
        .await
        .unwrap();
    assert!(matches!(next_msg(&mut late).await, Message::Text(_)));
    let Message::Binary(b) = next_msg(&mut late).await else {
        panic!("a frame")
    };
    assert!(
        Frame::parse(&b).unwrap().key,
        "a joining viewer starts at a keyframe"
    );

    let stats: serde_json::Value = reqwest::get(format!("http://{addr}/stats"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(stats["frames"].as_u64().unwrap() >= 60, "{stats}");
    assert!(
        stats["after_last_tile_ms"]["p50"].as_f64().is_some(),
        "{stats}"
    );
    cancel.cancel();
}

/// A region with nothing to stream (the EC2 half before any instance) still produces a
/// black stream on the slot clock, so a viewer always gets a picture.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_region_with_no_tiles_streams_black() {
    let shared = Shared::new();
    shared.set_layout(Layout {
        epoch_unix_ms: now_ms(),
        image_aspect: 16.0 / 9.0,
        region: Rect {
            x: 0.5,
            y: 0.0,
            w: 0.5,
            h: 1.0,
        },
        tiles: vec![TileSpec {
            id: "ec2-1".into(),
            rect: Rect {
                x: 0.5,
                y: 0.0,
                w: 0.5,
                h: 1.0,
            },
            url: None,
        }],
        generation: 1,
        from_frame: 0,
    });
    let cancel = CancellationToken::new();
    let opts = Options {
        image_height: 36,
        ..Options::default()
    };
    tokio::spawn(run(shared.clone(), Arc::new(Raw), opts, cancel.clone()));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, router(shared)).await.unwrap() });
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/frames"))
        .await
        .unwrap();
    let h: serde_json::Value = match next_msg(&mut ws).await {
        Message::Text(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("header first, got {other:?}"),
    };
    assert_eq!(
        (&h["x0"], &h["width"], &h["height"]),
        (&32.into(), &32.into(), &36.into())
    );
    for _ in 0..5 {
        let Message::Binary(b) = next_msg(&mut ws).await else {
            continue;
        };
        let f = Frame::parse(&b).unwrap();
        assert!(f
            .au
            .chunks(32 * 36)
            .enumerate()
            .all(|(p, plane)| plane.iter().all(|v| *v == [16, 128, 128][p])));
    }
    cancel.cancel();
}

/// A new layout generation (the region re-tiled) keeps the old composite until every tile
/// of the new generation has delivered a frame at or after the generation's frame, then
/// swaps them all at once: no frame mixes the generations, no new tile is drawn black for
/// want of its first frame, and a stream that changes height at that frame is followed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_generation_replaces_the_old_composite_all_at_once() {
    let epoch = now_ms() - 10_000;
    let queries = Arc::new(Mutex::new(Vec::new()));
    let head = |y0: u32, h: u32| Header {
        codec: "raw".into(),
        width: 64,
        height: h,
        full_width: 128,
        full_height: 72,
        x0: 0,
        y0,
        fps: FPS,
        crop: Crop {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
    };
    let tile = |header: Header, id_u: u8, from_k: i64, switch: Option<(i64, Header)>| FakeTile {
        header,
        id_u,
        epoch,
        queries: queries.clone(),
        from_k,
        switch,
        lag_ms: 0.0,
    };
    let region = Rect {
        x: 0.0,
        y: 0.0,
        w: 0.5,
        h: 1.0,
    };
    let layout = |generation: u64, from_frame: i64, tiles: Vec<(&str, Rect, String)>| Layout {
        epoch_unix_ms: epoch,
        image_aspect: 16.0 / 9.0,
        region,
        tiles: tiles
            .into_iter()
            .map(|(id, rect, url)| TileSpec {
                id: id.into(),
                rect,
                url: Some(url),
            })
            .collect(),
        generation,
        from_frame,
    };
    let shared = Shared::new();
    // Slot a draws the whole region until frame F, then the top half (its renderer sends a
    // new header with the new height on the same stream); F is 2 s ahead.
    let now_k = ((now_ms() - epoch) as f64 * FPS as f64 / 1000.0) as i64;
    let f = now_k + 60;
    let a = start_tile(tile(head(0, 72), 60, i64::MIN, Some((f, head(0, 36))))).await;
    shared.set_layout(layout(1, 0, vec![("slot-a", region, a.clone())]));
    let cancel = CancellationToken::new();
    let opts = Options {
        image_height: 72,
        encode: EncodeParams {
            keyint: 15,
            ..Default::default()
        },
        ..Options::default()
    };
    tokio::spawn(run(shared.clone(), Arc::new(Raw), opts, cancel.clone()));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let s2 = shared.clone();
    tokio::spawn(async move { axum::serve(l, router(s2)).await.unwrap() });
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/frames"))
        .await
        .unwrap();
    let header = |m: Message| -> serde_json::Value {
        match m {
            Message::Text(t) => serde_json::from_str(&t).unwrap(),
            other => panic!("a header, got {other:?}"),
        }
    };
    assert_eq!(
        header(next_msg(&mut ws).await)["tiles"],
        serde_json::json!(["slot-a"])
    );
    // Generation 2 splits the region into top and bottom at frame F; the bottom tile's
    // renderer only starts streaming half a second after F.
    let half = |y: f64| Rect {
        x: 0.0,
        y,
        w: 0.5,
        h: 0.5,
    };
    let c = start_tile(tile(head(36, 36), 120, f + 15, None)).await;
    shared.set_layout(layout(
        2,
        f,
        vec![("slot-a", half(0.0), a), ("slot-c", half(0.5), c)],
    ));
    let mut old_ts = Vec::new();
    loop {
        match next_msg(&mut ws).await {
            Message::Binary(bytes) => old_ts.push(Frame::parse(&bytes).unwrap().t),
            m => {
                assert_eq!(header(m)["tiles"], serde_json::json!(["slot-a", "slot-c"]));
                break;
            }
        }
    }
    let k_of = |t: f64| (t * FPS as f64).round() as i64;
    assert!(
        old_ts.iter().all(|t| k_of(*t) < f),
        "the old generation ran past its last frame {}: {:?}",
        f - 1,
        old_ts
    );
    assert_eq!(
        old_ts.last().map(|t| k_of(*t)),
        Some(f - 1),
        "the old composite is kept up to the new generation's frame"
    );
    let Message::Binary(bytes) = next_msg(&mut ws).await else {
        panic!("a frame after the new header")
    };
    let first = Frame::parse(&bytes).unwrap();
    assert!(
        k_of(first.t) >= f + 15,
        "the new generation started at {} before its right tile's first frame {}",
        k_of(first.t),
        f + 15
    );
    assert!(first.key, "the new generation starts at a keyframe");
    let pic = Picture {
        width: 64,
        height: 72,
        data: first.au,
    };
    assert_eq!(
        pic.get(10, 10)[1],
        60,
        "the top tile: the same stream at its new height"
    );
    assert_eq!(
        pic.get(10, 50)[1],
        120,
        "the bottom tile of the new generation, not black"
    );
    // The aggregator asked each stream for its tile's height in its own generation.
    let hs: Vec<String> = queries
        .lock()
        .unwrap()
        .iter()
        .filter_map(|q| q.get("h").cloned())
        .collect();
    assert!(
        hs.contains(&"72".to_string()) && hs.contains(&"36".to_string()),
        "{hs:?}"
    );
    cancel.cancel();
}

/// A re-tile whose tiles all stream already starts the new composite at the generation's
/// frame exactly, the frame after the old composite's last, even when one renderer runs
/// 200 ms behind the others: no slot of the image is skipped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_re_tile_of_streaming_tiles_skips_no_slot() {
    let epoch = now_ms() - 10_000;
    let queries = Arc::new(Mutex::new(Vec::new()));
    let head = |x0: u32, w: u32| Header {
        codec: "raw".into(),
        width: w,
        height: 72,
        full_width: 128,
        full_height: 72,
        x0,
        y0: 0,
        fps: FPS,
        crop: Crop {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
    };
    let tile = |header: Header, id_u: u8, lag_ms: f64| FakeTile {
        header,
        id_u,
        epoch,
        queries: queries.clone(),
        from_k: i64::MIN,
        switch: None,
        lag_ms,
    };
    let region = Rect {
        x: 0.0,
        y: 0.0,
        w: 0.5,
        h: 1.0,
    };
    let half = |x: f64| Rect {
        x,
        y: 0.0,
        w: 0.25,
        h: 1.0,
    };
    let a = start_tile(tile(head(0, 32), 60, 0.0)).await;
    let b = start_tile(tile(head(32, 32), 90, 200.0)).await;
    let spec = |id: &str, rect: Rect, url: &str| TileSpec {
        id: id.into(),
        rect,
        url: Some(url.into()),
    };
    let shared = Shared::new();
    shared.set_layout(Layout {
        epoch_unix_ms: epoch,
        image_aspect: 16.0 / 9.0,
        region,
        tiles: vec![
            spec("slot-a", half(0.0), &a),
            spec("slot-b", half(0.25), &b),
        ],
        generation: 1,
        from_frame: 0,
    });
    let cancel = CancellationToken::new();
    let opts = Options {
        image_height: 72,
        encode: EncodeParams {
            keyint: 15,
            ..Default::default()
        },
        ..Options::default()
    };
    tokio::spawn(run(shared.clone(), Arc::new(Raw), opts, cancel.clone()));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let s2 = shared.clone();
    tokio::spawn(async move { axum::serve(l, router(s2)).await.unwrap() });
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/frames"))
        .await
        .unwrap();
    assert!(matches!(next_msg(&mut ws).await, Message::Text(_)));
    // The same tiles in a new generation (as a re-tile elsewhere on the wall makes one).
    let f = ((now_ms() - epoch) as f64 * FPS as f64 / 1000.0) as i64 + 45;
    shared.set_layout(Layout {
        epoch_unix_ms: epoch,
        image_aspect: 16.0 / 9.0,
        region,
        tiles: vec![
            spec("slot-a", half(0.0), &a),
            spec("slot-b", half(0.25), &b),
        ],
        generation: 2,
        from_frame: f,
    });
    let k_of = |t: f64| (t * FPS as f64).round() as i64;
    let mut last_old = None;
    while let Message::Binary(bytes) = next_msg(&mut ws).await {
        last_old = Some(k_of(Frame::parse(&bytes).unwrap().t));
    }
    let Message::Binary(bytes) = next_msg(&mut ws).await else {
        panic!("a frame after the new header")
    };
    let first_new = k_of(Frame::parse(&bytes).unwrap().t);
    assert_eq!(
        last_old,
        Some(f - 1),
        "the old composite runs to the frame before"
    );
    assert_eq!(
        first_new, f,
        "the new composite starts at the generation's frame"
    );
    cancel.cancel();
}

/// A self-signed certificate for `name`, as the agent's `tls.json` holds it, and the
/// certificate alone (PEM) for a client to trust.
fn cert_file(name: &str) -> (String, String) {
    let ck = rcgen::generate_simple_self_signed(vec![name.to_string()]).unwrap();
    let body = serde_json::json!({"cert": ck.cert.pem(), "key": ck.signing_key.serialize_pem()});
    (body.to_string(), ck.cert.pem())
}

fn trusting(pem: &str) -> Arc<rustls::ClientConfig> {
    use rustls::pki_types::pem::PemObject;
    let mut roots = rustls::RootCertStore::empty();
    for c in rustls::pki_types::CertificateDer::pem_slice_iter(pem.as_bytes()) {
        roots.add(c.unwrap()).unwrap();
    }
    Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth(),
    )
}

/// Open `/frames` at `addr` over TLS as `wss://<name>/frames`, trusting only `pem`.
async fn open_wss(addr: std::net::SocketAddr, name: &str, pem: &str) -> anyhow::Result<Ws> {
    let tcp = tokio::net::TcpStream::connect(addr).await?;
    let (ws, _) = tokio_tungstenite::client_async_tls_with_config(
        format!("wss://{name}:{}/frames", addr.port()),
        tcp,
        None,
        Some(tokio_tungstenite::Connector::Rustls(trusting(pem))),
    )
    .await?;
    Ok(ws)
}

/// The public listener (`--tls-listen`) serves the region stream with the certificate
/// file's identity and a renewed file to the connections that follow; it does not serve
/// the layout or the stats, which stay on the private listener.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_public_listener_serves_frames_with_tls_and_no_control_routes() {
    use fcvm::fleet::aggregate::public_router;
    use fcvm::fleet::aggregate::tls::{CertFile, TlsListener};
    const NAME: &str = "agg.fleet.test";
    let shared = Shared::new();
    let region = Rect {
        x: 0.5,
        y: 0.0,
        w: 0.5,
        h: 1.0,
    };
    shared.set_layout(Layout {
        epoch_unix_ms: now_ms(),
        image_aspect: 16.0 / 9.0,
        region,
        tiles: vec![TileSpec {
            id: "ec2-1".into(),
            rect: region,
            url: None,
        }],
        generation: 1,
        from_frame: 0,
    });
    let cancel = CancellationToken::new();
    let opts = Options {
        image_height: 36,
        ..Options::default()
    };
    tokio::spawn(run(shared.clone(), Arc::new(Raw), opts, cancel.clone()));

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tls.json");
    let (first, first_pem) = cert_file(NAME);
    std::fs::write(&path, &first).unwrap();
    let certs = CertFile::load(&path).unwrap();
    let l = TlsListener::bind("127.0.0.1:0", certs.clone())
        .await
        .unwrap();
    let addr = l.address();
    let app = public_router(shared.clone());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

    let mut ws = open_wss(addr, NAME, &first_pem).await.unwrap();
    let h: serde_json::Value = match next_msg(&mut ws).await {
        Message::Text(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("header first, got {other:?}"),
    };
    assert_eq!((&h["x0"], &h["height"]), (&32.into(), &36.into()));
    assert!(matches!(next_msg(&mut ws).await, Message::Binary(_)));

    let http = |pem: &str| {
        reqwest::Client::builder()
            .use_rustls_tls()
            .tls_built_in_root_certs(false)
            .add_root_certificate(reqwest::Certificate::from_pem(pem.as_bytes()).unwrap())
            .resolve(NAME, addr)
            .no_proxy()
            .build()
            .unwrap()
    };
    let base = format!("https://{NAME}:{}", addr.port());
    let c = http(&first_pem);
    let r = c.get(format!("{base}/healthz")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "ok");
    for (method, path) in [("GET", "/layout"), ("POST", "/layout"), ("GET", "/stats")] {
        let r = c
            .request(method.parse().unwrap(), format!("{base}{path}"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404, "{method} {path} over the public listener");
    }

    // A renewal: the next connection gets the new certificate, and one that trusts only
    // the old certificate is refused.
    let (second, second_pem) = cert_file(NAME);
    std::fs::write(&path, &second).unwrap();
    assert!(certs.reload().unwrap(), "a new file is a new version");
    assert!(!certs.reload().unwrap(), "the same file is not");
    open_wss(addr, NAME, &second_pem).await.unwrap();
    assert!(open_wss(addr, NAME, &first_pem).await.is_err());
    // A file that does not load keeps the version in use.
    std::fs::write(&path, "{}").unwrap();
    assert!(certs.reload().is_err());
    open_wss(addr, NAME, &second_pem).await.unwrap();
    // The stream that was open before the renewal keeps going.
    assert!(matches!(next_msg(&mut ws).await, Message::Binary(_)));
    cancel.cancel();
}
