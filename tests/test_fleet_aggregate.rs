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

async fn serve_tile(mut s: WebSocket, t: FakeTile) {
    if s.send(AxMessage::Text(
        serde_json::to_string(&t.header).unwrap().into(),
    ))
    .await
    .is_err()
    {
        return;
    }
    let mut k = ((now_ms() - t.epoch) as f64 * FPS as f64 / 1000.0).ceil() as i64;
    loop {
        let due = t.epoch as f64 + k as f64 * 1000.0 / FPS as f64;
        let wait = due - now_ms() as f64;
        if wait > 0.0 {
            tokio::time::sleep(Duration::from_micros((wait * 1000.0) as u64)).await;
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
