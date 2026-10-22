//! The coordinator over real WebSockets: snapshots on connect, monotonic versions,
//! receipts, repeated intent ids, two viewers agreeing, the text check, and an agent
//! connection driven by browser intents.

use fcvm::fleet::coord::text_check::{FakeCheck, TextChecker};
use fcvm::fleet::coord::{agents, live, start, store, Handle};
use fcvm::fleet::lines::{spawn_writer, LineReader};
use fleet_core::protocol::{AgentMsg, CoordMsg, SlotReport};
use fleet_core::state::Gpu;
use fleet_core::{Config, Event, State};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn coordinator(checker: Arc<dyn TextChecker>) -> (String, Handle) {
    let (handle, _actor) = start(State::new(Config::default(), 1_000), None, None);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/live", listener.local_addr().unwrap());
    let h = handle.clone();
    tokio::spawn(async move {
        live::serve(listener, h, checker, vec!["https://wall.example".into()])
            .await
            .unwrap()
    });
    (url, handle)
}

fn allow_all() -> Arc<dyn TextChecker> {
    Arc::new(FakeCheck {
        refuse: vec![],
        fail: false,
        delay: Duration::ZERO,
    })
}

/// A browser: every message it receives, snapshots kept by version.
struct Viewer {
    ws: Ws,
    snapshots: BTreeMap<u64, Value>,
    order: Vec<u64>,
    receipts: Vec<Value>,
    other: Vec<Value>,
}

impl Viewer {
    async fn connect(url: &str) -> Viewer {
        let (ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        Viewer {
            ws,
            snapshots: BTreeMap::new(),
            order: Vec::new(),
            receipts: Vec::new(),
            other: Vec::new(),
        }
    }

    async fn send(&mut self, v: Value) {
        self.ws
            .send(Message::Text(v.to_string().into()))
            .await
            .unwrap();
    }

    async fn intent(&mut self, id: &str, name: &str, args: Value) {
        self.send(json!({"type": "intent", "id": id, "name": name, "args": args}))
            .await;
    }

    async fn recv(&mut self) -> Value {
        loop {
            let m = tokio::time::timeout(Duration::from_secs(10), self.ws.next())
                .await
                .expect("a message within 10 s")
                .unwrap()
                .unwrap();
            if let Message::Text(t) = m {
                let v: Value = serde_json::from_str(&t).unwrap();
                match v["type"].as_str().unwrap() {
                    "snapshot" => {
                        let version = v["version"].as_u64().unwrap();
                        self.order.push(version);
                        self.snapshots.insert(version, v["state"].clone());
                    }
                    "receipt" => self.receipts.push(v.clone()),
                    _ => self.other.push(v.clone()),
                }
                return v;
            }
        }
    }

    async fn receipt(&mut self, id: &str) -> Value {
        loop {
            if let Some(r) = self.receipts.iter().find(|r| r["id"] == id) {
                return r.clone();
            }
            self.recv().await;
        }
    }

    /// Read until a snapshot at or past `version` arrives; returns its state.
    async fn at_least(&mut self, version: u64) -> Value {
        loop {
            if let Some((_, s)) = self.snapshots.range(version..).next_back() {
                return s.clone();
            }
            self.recv().await;
        }
    }

    fn latest(&self) -> (u64, &Value) {
        let (v, s) = self.snapshots.iter().next_back().unwrap();
        (*v, s)
    }
}

#[tokio::test]
async fn a_viewer_gets_a_snapshot_on_connect_and_receipts_with_rising_versions() {
    let (url, _h) = coordinator(allow_all()).await;
    let mut a = Viewer::connect(&url).await;
    let hello = a.recv().await;
    assert_eq!(hello["type"], "hello", "the first message names the viewer");
    assert!(hello["viewer"].as_str().unwrap().starts_with("v-"));
    let first = a.recv().await;
    assert_eq!(first["type"], "snapshot");
    for key in [
        "tiles",
        "fleet",
        "scene",
        "settings",
        "relaunch_all",
        "viewers",
        "actions",
    ] {
        assert!(first["state"].get(key).is_some(), "snapshot lacks {key}");
    }
    let mut last = 0;
    for (i, v) in [0.125, 0.25, 0.5].iter().enumerate() {
        let id = format!("t-{i}");
        a.intent(&id, "time_of_day", json!({"v": v})).await;
        let r = a.receipt(&id).await;
        assert_eq!(r["outcome"], "committed", "{r}");
        let version = r["version"].as_u64().unwrap();
        assert!(
            version > last,
            "receipt versions must rise: {version} after {last}"
        );
        last = version;
        let s = a.at_least(version).await;
        assert_eq!(s["scene"]["time_of_day"], *v);
        let mine = s["actions"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(
            mine["viewer"], hello["viewer"],
            "the action names this viewer"
        );
        assert_eq!(mine["id"], id.as_str());
        assert_eq!(mine["args"]["v"], *v);
    }
    assert!(
        a.order.windows(2).all(|w| w[0] < w[1]),
        "snapshot versions went back: {:?}",
        a.order
    );
    a.intent("bad-1", "time_of_day", json!({"v": 1.5})).await;
    let r = a.receipt("bad-1").await;
    assert_eq!(r["outcome"], "refused");
    assert_eq!(
        r["reason"],
        "args.v must be a fraction of the day in [0, 1)"
    );
}

#[tokio::test]
async fn a_repeated_intent_id_returns_its_first_receipt_and_applies_once() {
    let (url, _h) = coordinator(allow_all()).await;
    let mut a = Viewer::connect(&url).await;
    a.recv().await;
    a.intent("same", "time_of_day", json!({"v": 0.25})).await;
    let first = a.receipt("same").await;
    // The retry carries a different value; it must change nothing.
    a.intent("same", "time_of_day", json!({"v": 0.75})).await;
    a.intent("marker", "time_of_day", json!({"v": 0.375})).await;
    let marker = a.receipt("marker").await;
    let repeats: Vec<&Value> = a.receipts.iter().filter(|r| r["id"] == "same").collect();
    assert_eq!(repeats.len(), 2);
    assert_eq!(
        repeats[0], repeats[1],
        "a repeated id must get the first receipt"
    );
    assert_eq!(
        first["version"].as_u64().unwrap() + 1,
        marker["version"].as_u64().unwrap()
    );
    let s = a.at_least(marker["version"].as_u64().unwrap()).await;
    let named: Vec<&Value> = s["actions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|x| x["name"] == "time_of_day")
        .collect();
    assert_eq!(named.len(), 2, "the repeat must not be applied: {named:?}");
    // A word retried with the same id also gets its first receipt, without a second check.
    a.intent("w", "word", json!({"text": "hello"})).await;
    let w1 = a.receipt("w").await;
    a.intent("w", "word", json!({"text": "hello"})).await;
    a.intent("marker2", "time_of_day", json!({"v": 0.625}))
        .await;
    a.receipt("marker2").await;
    let ws: Vec<&Value> = a.receipts.iter().filter(|r| r["id"] == "w").collect();
    assert_eq!(ws.len(), 2);
    assert_eq!(ws[0], &w1);
    assert_eq!(ws[1], &w1);
}

#[tokio::test]
async fn two_viewers_see_the_same_state_at_every_version_both_saw() {
    let (url, _h) = coordinator(allow_all()).await;
    let mut a = Viewer::connect(&url).await;
    let mut b = Viewer::connect(&url).await;
    a.recv().await;
    b.recv().await;
    for i in 0..20 {
        let (who, id) = if i % 2 == 0 {
            (&mut a, format!("a-{i}"))
        } else {
            (&mut b, format!("b-{i}"))
        };
        who.intent(&id, "time_of_day", json!({"v": i as f64 / 32.0}))
            .await;
        who.intent(
            &format!("{id}-l"),
            "camera",
            json!({"drag": {"yaw_deg": 5.0, "pitch_deg": 0.0}}),
        )
        .await;
    }
    let ra = a.receipt("a-18-l").await["version"].as_u64().unwrap();
    let rb = b.receipt("b-19-l").await["version"].as_u64().unwrap();
    let end = ra.max(rb);
    a.at_least(end).await;
    b.at_least(end).await;
    let mut common = 0;
    for (v, sa) in &a.snapshots {
        if let Some(sb) = b.snapshots.get(v) {
            // `viewers` counts both once both joined; everything else must agree too.
            assert_eq!(sa, sb, "viewers disagree at version {v}");
            common += 1;
        }
    }
    assert!(common >= 1);
    let (va, sa) = a.latest();
    let (vb, sb) = b.latest();
    assert_eq!(va, vb);
    assert_eq!(sa, sb);
    assert_eq!(sa["viewers"]["count"], 2);
    assert_eq!(sa["scene"]["time_of_day"], 19.0 / 32.0);
    for v in [&a, &b] {
        assert!(v.order.windows(2).all(|w| w[0] < w[1]), "{:?}", v.order);
    }
}

#[tokio::test]
async fn words_need_an_allow_from_the_text_check() {
    let checker: Arc<dyn TextChecker> = Arc::new(FakeCheck {
        refuse: vec!["nasty".into()],
        fail: false,
        delay: Duration::from_millis(50),
    });
    let (url, _h) = coordinator(checker).await;
    let mut a = Viewer::connect(&url).await;
    let mut b = Viewer::connect(&url).await;
    a.recv().await;
    a.intent("w1", "word", json!({"text": "hello"})).await;
    let r = a.receipt("w1").await;
    assert_eq!(r["outcome"], "committed", "{r}");
    assert_eq!(r["check"]["verdict"], "allow");
    b.intent("w2", "word", json!({"text": "so nasty"})).await;
    let r = b.receipt("w2").await;
    assert_eq!(r["outcome"], "refused");
    assert_eq!(r["check"]["verdict"], "refuse");
    // Rate limited before the check: no verdict.
    a.intent("w3", "word", json!({"text": "again"})).await;
    let r = a.receipt("w3").await;
    assert_eq!(r["reason"], "one word per 5 s per viewer");
    assert!(r.get("check").is_none());
    a.intent("w4", "word", json!({"text": "x".repeat(30)}))
        .await;
    assert_eq!(a.receipt("w4").await["outcome"], "refused");
    let s = a.at_least(r["version"].as_u64().unwrap()).await;
    let text = s.to_string();
    assert!(text.contains("hello"));
    assert!(
        !text.contains("nasty"),
        "a refused word reached the snapshot"
    );

    // A checker that is down refuses everything.
    let down: Arc<dyn TextChecker> = Arc::new(FakeCheck {
        refuse: vec![],
        fail: true,
        delay: Duration::ZERO,
    });
    let (url, _h) = coordinator(down).await;
    let mut c = Viewer::connect(&url).await;
    c.intent("w", "word", json!({"text": "hello"})).await;
    let r = c.receipt("w").await;
    assert_eq!(r["outcome"], "refused");
    assert_eq!(r["check"]["verdict"], "error");
}

#[tokio::test]
async fn pings_get_the_server_clock_and_garbage_gets_an_error() {
    let (url, _h) = coordinator(allow_all()).await;
    let mut a = Viewer::connect(&url).await;
    a.send(json!({"type": "ping", "client_unix_ms": 123.5}))
        .await;
    a.send(json!({"type": "nonsense"})).await;
    while a.other.len() < 3 {
        a.recv().await;
    }
    let clock = a.other.iter().find(|m| m["type"] == "clock").unwrap();
    assert_eq!(clock["client_unix_ms"], 123.5);
    assert!(clock["server_unix_ms"].as_u64().unwrap() > 1_700_000_000_000);
    assert!(a.other.iter().any(|m| m["type"] == "error"));
}

/// The coordinator's side of an agent connection, with the test playing the agent.
struct FakeAgent {
    to_coord: tokio::sync::mpsc::UnboundedSender<AgentMsg>,
    from_coord: LineReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    conn: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl FakeAgent {
    fn connect(handle: &Handle, host: &str, gpus: usize) -> FakeAgent {
        let (coord_side, agent_side) = tokio::io::duplex(1 << 20);
        let (ci, co) = tokio::io::split(coord_side);
        let (ai, ao) = tokio::io::split(agent_side);
        let h = handle.clone();
        let conn = tokio::spawn(async move { agents::serve_connection(ci, co, &h).await });
        let (to_coord, _w) = spawn_writer::<AgentMsg, _>(ao, "coordinator");
        to_coord
            .send(AgentMsg::Hello {
                host: host.into(),
                arch: "x86_64".into(),
                gpus: (0..gpus)
                    .map(|i| Gpu {
                        sbdf: format!("0000:{:02x}:00.0", i + 1),
                        name: "Tesla T4".into(),
                    })
                    .collect(),
                capacity: gpus,
            })
            .unwrap();
        FakeAgent {
            to_coord,
            from_coord: LineReader::new(ai, "coordinator"),
            conn,
        }
    }

    async fn next(&mut self) -> CoordMsg {
        tokio::time::timeout(Duration::from_secs(10), self.from_coord.next::<CoordMsg>())
            .await
            .expect("coordinator message within 10 s")
            .unwrap()
            .expect("connection open")
    }

    async fn assign(&mut self) -> Vec<fleet_core::protocol::AssignSlot> {
        loop {
            if let CoordMsg::Assign { slots, .. } = self.next().await {
                return slots;
            }
        }
    }

    fn report(&self, id: &str, phase: &str, generation: u64) {
        self.to_coord
            .send(AgentMsg::Slot(SlotReport {
                id: id.into(),
                phase: phase.into(),
                generation,
                vm: Some(format!("fleet-{id}")),
                frame_url: Some(format!("ws://gpu-host.example:2000{}/frames", &id[5..])),
                timings: [("gpu_ms".to_string(), 3000u64)].into_iter().collect(),
                error: None,
            }))
            .unwrap();
    }
}

#[tokio::test]
async fn browser_intents_reach_the_agent_as_assignments() {
    let (url, handle) = coordinator(allow_all()).await;
    let mut agent = FakeAgent::connect(&handle, "gpu-a", 2);
    // Scene first, then the assignment.
    assert!(matches!(agent.next().await, CoordMsg::Scene(_)));
    let slots = agent.assign().await;
    assert_eq!(slots.len(), 2);
    assert_eq!(slots[0].id, "slot-1");
    assert_eq!(slots[0].generation, 1);
    assert!(slots[0].rect.is_some());
    agent.report("slot-1", "running", 1);
    agent.report("slot-2", "running", 1);

    let mut v = Viewer::connect(&url).await;
    v.intent("r", "relaunch", json!({"tile": "fcvm:1"})).await;
    let r = v.receipt("r").await;
    assert_eq!(r["outcome"], "committed", "{r}");
    let slots = agent.assign().await;
    assert_eq!(slots[0].generation, 2);
    assert_eq!(slots[1].generation, 1);
    agent.report("slot-1", "running", 2);
    let s = loop {
        let s = v.at_least(r["version"].as_u64().unwrap()).await;
        if s["tiles"][0]["last_relaunch"]["first_frame_at"].is_u64() {
            break s;
        }
        v.recv().await;
    };
    assert_eq!(s["tiles"][0]["phase"], "running");
    assert_eq!(s["tiles"][0]["last_relaunch"]["phases"]["gpu_ms"], 3000);
    assert_eq!(
        s["tiles"][0]["frame_url"],
        "ws://gpu-host.example:20001/frames"
    );

    // Relaunch all bumps both; a camera move sends a new scene.
    v.intent("ra", "relaunch_all", json!({})).await;
    v.receipt("ra").await;
    let slots = agent.assign().await;
    assert_eq!((slots[0].generation, slots[1].generation), (3, 2));
    v.intent(
        "cam",
        "camera",
        json!({"drag": {"yaw_deg": 10.0, "pitch_deg": 0.0}}),
    )
    .await;
    v.receipt("cam").await;
    loop {
        if let CoordMsg::Scene(s) = agent.next().await {
            assert!(s.camera.len() >= 2);
            break;
        }
    }

    // A second connection naming the same host is refused.
    let dup = FakeAgent::connect(&handle, "gpu-a", 1);
    let err = tokio::time::timeout(Duration::from_secs(10), dup.conn)
        .await
        .unwrap()
        .unwrap();
    assert!(err.is_err(), "a second agent for one host must be refused");

    // The agent going away takes its slots out of the tiling.
    drop(agent.to_coord);
    agent.conn.await.unwrap().unwrap();
    loop {
        let (_, s) = v.latest();
        if s["fleet"]["n"] == 0 {
            assert_eq!(s["tiles"], json!([]));
            break;
        }
        v.recv().await;
    }
}

/// A cross-origin browser is refused unless its origin is configured; same-origin pages
/// and clients that send no Origin (not browsers) are served.
#[tokio::test]
async fn live_refuses_origins_it_was_not_given() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let (url, _h) = coordinator(allow_all()).await;
    let host = url
        .trim_start_matches("ws://")
        .trim_end_matches("/live")
        .to_string();
    let with_origin = |o: &str| {
        let mut r = url.as_str().into_client_request().unwrap();
        r.headers_mut().insert("origin", o.parse().unwrap());
        r
    };
    match tokio_tungstenite::connect_async(with_origin("https://evil.example")).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status(), 403)
        }
        Err(e) => panic!("a foreign origin must get 403, got {e}"),
        Ok(_) => panic!("a foreign origin must get 403, got a socket"),
    }
    let tricky =
        tokio_tungstenite::connect_async(with_origin(&format!("http://{host}.evil.example"))).await;
    assert!(tricky.is_err(), "a look-alike origin must be refused");
    for ok in ["https://wall.example".to_string(), format!("http://{host}")] {
        let (mut ws, _) = tokio_tungstenite::connect_async(with_origin(&ok))
            .await
            .unwrap_or_else(|e| panic!("{ok} must be allowed: {e}"));
        let first = ws.next().await.unwrap().unwrap();
        assert!(first.to_text().unwrap().contains("hello"));
    }
    let (mut ws, _) = tokio_tungstenite::connect_async(url.as_str())
        .await
        .unwrap();
    assert!(ws.next().await.unwrap().is_ok());
}

/// Stopping the coordinator writes the newest state, even when it changed less than a
/// write interval (1 s) ago.
#[tokio::test]
async fn shutdown_writes_the_last_state_to_the_state_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fleet.json");
    let (handle, actor) = start(
        State::new(Config::default(), 1_000),
        Some(store::spawn_writer(path.clone())),
        None,
    );
    let mut last = 0;
    for i in 0..5 {
        let r = handle
            .apply(Event::Intent {
                at: 0,
                viewer: "v-1".into(),
                id: format!("t-{i}"),
                name: "time_of_day".into(),
                args: json!({"v": 0.1 * i as f64}),
                check: None,
            })
            .await
            .unwrap();
        last = r.version;
    }
    handle.shutdown().await;
    let saved = store::load(&path).unwrap().expect("a state file");
    assert_eq!(
        saved.version, last,
        "the state file misses the last changes"
    );
    assert!(saved.receipts.contains_key("t-4"));
    tokio::time::timeout(Duration::from_secs(5), actor)
        .await
        .unwrap()
        .unwrap();
}
