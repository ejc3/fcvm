//! The coordinator over real WebSockets: snapshots on connect, monotonic versions,
//! receipts, repeated intent ids, two viewers agreeing, the text check over the whole sky,
//! Turnstile, per-address limits, a borrowed GPU told to its viewer alone, the poster
//! frame, and an agent connection driven by browser intents.

use fcvm::fleet::aggregate::codec::Raw;
use fcvm::fleet::aggregate::frame::Frame;
use fcvm::fleet::coord::text_check::{FakeCheck, TextChecker};
use fcvm::fleet::coord::turnstile::{self, Verifier};
use fcvm::fleet::coord::{agents, ec2, live, poster, start, store, Handle};
use fcvm::fleet::lines::{spawn_writer, LineReader};
use fcvm::fleet::now_ms;
use fleet_core::protocol::{AgentMsg, CoordMsg, SlotReport};
use fleet_core::state::Gpu;
use fleet_core::{Config, Event, State};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Opts {
    config: Config,
    checker: Arc<dyn TextChecker>,
    verifier: Arc<dyn Verifier>,
    trust_cf: bool,
    /// The EC2 aggregator as the coordinator reaches it.
    ec2_local: Option<String>,
    site_key: Option<String>,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            config: Config {
                light_interval_ms: 0,
                ..Config::default()
            },
            checker: allow_all(),
            verifier: Arc::new(turnstile::Off),
            trust_cf: true,
            ec2_local: None,
            site_key: None,
        }
    }
}

struct Coord {
    url: String,
    handle: Handle,
}

async fn coordinator_with(o: Opts) -> Coord {
    let (handle, _actor) = start(
        State::new(o.config, now_ms()),
        None,
        None,
        o.ec2_local,
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg = live::LiveConfig {
        checker: o.checker,
        allowed_origins: vec!["https://wall.example".into()],
        verifier: o.verifier,
        trust_cf_connecting_ip: o.trust_cf,
        turnstile_site_key: o.site_key,
        poster: Arc::new(poster::Poster::default()),
    };
    let h = handle.clone();
    tokio::spawn(async move { live::serve(listener, h, cfg).await.unwrap() });
    Coord {
        url: format!("ws://{addr}/live"),
        handle,
    }
}

async fn coordinator(checker: Arc<dyn TextChecker>) -> (String, Handle) {
    let c = coordinator_with(Opts {
        checker,
        ..Opts::default()
    })
    .await;
    (c.url, c.handle)
}

fn allow_all() -> Arc<dyn TextChecker> {
    Arc::new(FakeCheck {
        refuse: vec![],
        fail: false,
        delay: Duration::ZERO,
    })
}

fn light(x: f64) -> Value {
    json!({"pos": [x, 2.0, -90.0], "color": [1.0, 0.5, 0.0]})
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
        Viewer::connect_from(url, None).await
    }

    /// Connect as a client behind Cloudflare at `ip` (the `CF-Connecting-IP` header).
    async fn connect_from(url: &str, ip: Option<&str>) -> Viewer {
        let mut req = url.into_client_request().unwrap();
        if let Some(ip) = ip {
            req.headers_mut()
                .insert("cf-connecting-ip", ip.parse().unwrap());
        }
        let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
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

    /// The `n`th receipt for `id` (a refusal that was not recorded can be followed by one
    /// that was).
    async fn nth_receipt(&mut self, id: &str, n: usize) -> Value {
        loop {
            if let Some(r) = self.receipts.iter().filter(|r| r["id"] == id).nth(n) {
                return r.clone();
            }
            self.recv().await;
        }
    }

    async fn message(&mut self, kind: &str) -> Value {
        self.message_where(kind, |_| true).await
    }

    async fn message_where(&mut self, kind: &str, f: impl Fn(&Value) -> bool) -> Value {
        loop {
            if let Some(m) = self.other.iter().find(|m| m["type"] == kind && f(m)) {
                return m.clone();
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

    /// Read until the latest snapshot satisfies `f`.
    async fn until(&mut self, f: impl Fn(&Value) -> bool) -> Value {
        loop {
            if let Some((_, s)) = self.snapshots.iter().next_back() {
                if f(s) {
                    return s.clone();
                }
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
    assert_eq!(hello["turnstile"], Value::Null, "no Turnstile configured");
    let first = a.recv().await;
    assert_eq!(first["type"], "snapshot");
    for key in [
        "tiles",
        "aggregates",
        "fleet",
        "scene",
        "next_race_at",
        "cooldown_until",
        "relaunch_all",
        "scoreboard",
        "races",
        "nudges",
        "borrow",
        "failures",
        "claims",
        "viewers",
        "actions",
    ] {
        assert!(first["state"].get(key).is_some(), "snapshot lacks {key}");
    }
    let mut last = 0;
    for i in 0..3 {
        let id = format!("l-{i}");
        a.intent(&id, "light", light(i as f64)).await;
        let r = a.receipt(&id).await;
        assert_eq!(r["outcome"], "committed", "{r}");
        let version = r["version"].as_u64().unwrap();
        assert!(
            version > last,
            "receipt versions must rise: {version} after {last}"
        );
        last = version;
        let s = a.at_least(version).await;
        assert_eq!(s["scene"]["lights"][i]["pos"][0], i as f64);
        let mine = s["actions"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(
            mine["viewer"], hello["viewer"],
            "the action names this viewer"
        );
        assert_eq!(mine["id"], id.as_str());
    }
    assert!(
        a.order.windows(2).all(|w| w[0] < w[1]),
        "snapshot versions went back: {:?}",
        a.order
    );
    a.intent(
        "bad-1",
        "light",
        json!({"pos": [1.0e9, 0.0, 0.0], "color": [1, 1, 1]}),
    )
    .await;
    let r = a.receipt("bad-1").await;
    assert_eq!(r["outcome"], "refused");
    assert_eq!(r["reason"], "pos must be within 1000000");
    // The camera is the director's: viewers cannot steer the wall or set its time of day.
    for name in ["camera", "time_of_day", "relaunch_all", "set_auto_relaunch"] {
        a.intent(name, name, json!({})).await;
        let r = a.receipt(name).await;
        assert_eq!(r["reason"], format!("unknown intent {name:?}"), "{r}");
    }
}

#[tokio::test]
async fn a_repeated_intent_id_returns_its_first_receipt_and_applies_once() {
    let (url, _h) = coordinator(allow_all()).await;
    let mut a = Viewer::connect(&url).await;
    a.recv().await;
    a.intent("same", "light", light(1.0)).await;
    let first = a.receipt("same").await;
    // The retry carries a different value; it must change nothing.
    a.intent("same", "light", light(2.0)).await;
    a.intent("marker", "light", light(3.0)).await;
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
        .filter(|x| x["name"] == "light")
        .collect();
    assert_eq!(named.len(), 2, "the repeat must not be applied: {named:?}");
    // A word retried with the same id also gets its first receipt, without a second check.
    a.intent("w", "word", json!({"text": "hello"})).await;
    let w1 = a.receipt("w").await;
    a.intent("w", "word", json!({"text": "hello"})).await;
    a.intent("marker2", "light", light(4.0)).await;
    a.receipt("marker2").await;
    let ws: Vec<&Value> = a.receipts.iter().filter(|r| r["id"] == "w").collect();
    assert_eq!(ws.len(), 2);
    assert_eq!(ws[0], &w1);
    assert_eq!(ws[1], &w1);
}

#[tokio::test]
async fn two_viewers_see_the_same_state_at_every_version_both_saw() {
    let (url, _h) = coordinator(allow_all()).await;
    let mut a = Viewer::connect_from(&url, Some("198.51.100.1")).await;
    let mut b = Viewer::connect_from(&url, Some("198.51.100.2")).await;
    a.recv().await;
    b.recv().await;
    for i in 0..20 {
        let (who, id) = if i % 2 == 0 {
            (&mut a, format!("a-{i}"))
        } else {
            (&mut b, format!("b-{i}"))
        };
        who.intent(&id, "light", light(i as f64)).await;
        who.intent(&format!("{id}-n"), "nudge", json!({"target": "coast"}))
            .await;
    }
    let ra = a.receipt("a-18-n").await["version"].as_u64().unwrap();
    let rb = b.receipt("b-19-n").await["version"].as_u64().unwrap();
    let end = ra.max(rb);
    a.at_least(end).await;
    b.at_least(end).await;
    let mut common = 0;
    for (v, sa) in &a.snapshots {
        if let Some(sb) = b.snapshots.get(v) {
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
    // Three lights per address: the newest three of each.
    let xs: Vec<f64> = sa["scene"]["lights"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["pos"][0].as_f64().unwrap())
        .collect();
    assert_eq!(xs, vec![14.0, 15.0, 16.0, 17.0, 18.0, 19.0]);
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
    let mut a = Viewer::connect_from(&url, Some("198.51.100.1")).await;
    let mut b = Viewer::connect_from(&url, Some("198.51.100.2")).await;
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
    assert_eq!(r["reason"], "one word per 5 s per client");
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
}

/// A word is judged together with the words already in the sky, so two harmless words
/// cannot make an unacceptable phrase.
#[tokio::test]
async fn a_word_is_checked_with_every_word_in_the_sky() {
    let checker: Arc<dyn TextChecker> = Arc::new(FakeCheck {
        refuse: vec!["bad word".into()],
        fail: false,
        delay: Duration::ZERO,
    });
    let (url, _h) = coordinator(checker).await;
    let mut a = Viewer::connect_from(&url, Some("198.51.100.1")).await;
    let mut b = Viewer::connect_from(&url, Some("198.51.100.2")).await;
    a.intent("w1", "word", json!({"text": "bad"})).await;
    assert_eq!(a.receipt("w1").await["outcome"], "committed");
    b.intent("w2", "word", json!({"text": "word"})).await;
    let r = b.receipt("w2").await;
    assert_eq!(r["outcome"], "refused", "{r}");
    assert_eq!(r["check"]["verdict"], "refuse");
    assert_eq!(r["check"]["sky"], json!(["bad"]));
}

/// A text check that cannot run refuses the word and marks the coordinator degraded, so
/// the page can say why words are not appearing.
#[tokio::test]
async fn a_failing_text_check_degrades_the_coordinator() {
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
    let s = c
        .until(|s| {
            s["failures"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["kind"] == "coordinator_degraded")
        })
        .await;
    let f = s["failures"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["kind"] == "coordinator_degraded")
        .unwrap()
        .clone();
    assert_eq!(f["component"], "text_check");
    assert!(f["reason"].as_str().unwrap().contains("down"), "{f}");
}

/// Limits are per client address: `CF-Connecting-IP` when the coordinator trusts it (it
/// sits behind Cloudflare), else the peer address.
#[tokio::test]
async fn limits_are_keyed_by_the_client_address() {
    let c = coordinator_with(Opts::default()).await;
    let mut a = Viewer::connect_from(&c.url, Some("198.51.100.1")).await;
    let mut b = Viewer::connect_from(&c.url, Some("198.51.100.2")).await;
    let mut a2 = Viewer::connect_from(&c.url, Some("198.51.100.1")).await;
    a.intent("w1", "word", json!({"text": "one"})).await;
    assert_eq!(a.receipt("w1").await["outcome"], "committed");
    b.intent("w2", "word", json!({"text": "two"})).await;
    assert_eq!(b.receipt("w2").await["outcome"], "committed");
    // A second connection from the first address shares its limit.
    a2.intent("w3", "word", json!({"text": "three"})).await;
    assert_eq!(
        a2.receipt("w3").await["reason"],
        "one word per 5 s per client"
    );

    // Not trusted: the header is ignored and both connections are 127.0.0.1.
    let c = coordinator_with(Opts {
        trust_cf: false,
        ..Opts::default()
    })
    .await;
    let mut a = Viewer::connect_from(&c.url, Some("198.51.100.1")).await;
    let mut b = Viewer::connect_from(&c.url, Some("198.51.100.2")).await;
    a.intent("w1", "word", json!({"text": "one"})).await;
    assert_eq!(a.receipt("w1").await["outcome"], "committed");
    b.intent("w2", "word", json!({"text": "two"})).await;
    assert_eq!(
        b.receipt("w2").await["reason"],
        "one word per 5 s per client"
    );
}

/// With Turnstile on, a connection's intents are refused (and not recorded) until it has
/// sent a token the verifier accepts.
#[tokio::test]
async fn intents_wait_for_turnstile() {
    let c = coordinator_with(Opts {
        verifier: Arc::new(turnstile::Fake {
            accept: ["good".to_string()].into_iter().collect(),
        }),
        site_key: Some("site-key-for-tests".into()),
        ..Opts::default()
    })
    .await;
    let mut a = Viewer::connect(&c.url).await;
    let hello = a.recv().await;
    assert_eq!(hello["turnstile"], "site-key-for-tests", "{hello}");
    a.intent("l", "light", light(1.0)).await;
    let r = a.nth_receipt("l", 0).await;
    assert_eq!(r["outcome"], "refused");
    assert_eq!(r["reason"], "verify first");
    a.send(json!({"type": "verify", "token": "bad"})).await;
    let v = a.message("verified").await;
    assert_eq!(v["ok"], false);
    a.other.clear();
    a.send(json!({"type": "verify", "token": "good"})).await;
    let v = a.message("verified").await;
    assert_eq!(v["ok"], true, "{v}");
    // The refusal was not recorded, so the same id now goes through.
    a.intent("l", "light", light(1.0)).await;
    let r = a.nth_receipt("l", 1).await;
    assert_eq!(r["outcome"], "committed", "{r}");
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
                retry_in_ms: None,
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

    // restart_now races every tile; a light sends a new scene.
    v.intent("rn", "restart_now", json!({})).await;
    assert_eq!(v.receipt("rn").await["outcome"], "committed");
    let slots = agent.assign().await;
    assert_eq!((slots[0].generation, slots[1].generation), (3, 2));
    v.intent("l", "light", light(5.0)).await;
    v.receipt("l").await;
    loop {
        if let CoordMsg::Scene(s) = agent.next().await {
            if !s.lights.is_empty() {
                break;
            }
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

/// With every GPU drawing the wall, `claim_gpu` borrows a running tile's GPU. Only the
/// claiming viewer learns its frame URL and session token; everyone sees the tile as
/// borrowed; the cap refuses a second claim; the viewer leaving returns the GPU.
#[tokio::test]
async fn a_claim_borrows_a_tile_and_only_its_viewer_gets_the_frames() {
    let (url, handle) = coordinator(allow_all()).await;
    let mut agent = FakeAgent::connect(&handle, "gpu-a", 2);
    agent.assign().await;
    agent.report("slot-1", "running", 1);
    agent.report("slot-2", "running", 1);
    let mut a = Viewer::connect_from(&url, Some("198.51.100.1")).await;
    let mut b = Viewer::connect_from(&url, Some("198.51.100.2")).await;
    b.until(|s| s["tiles"][1]["phase"] == "running").await;

    a.intent("c1", "claim_gpu", json!({})).await;
    let r = a.receipt("c1").await;
    assert_eq!(r["outcome"], "committed", "{r}");
    let slots = loop {
        let slots = agent.assign().await;
        if slots[1].private_camera.is_some() {
            break slots;
        }
    };
    assert_eq!(slots[1].generation, 2);
    assert_eq!(
        slots[1].rect, None,
        "a borrowed tile leaves its host's band"
    );
    assert!(slots[0].private_camera.is_none());

    b.intent("c2", "claim_gpu", json!({})).await;
    assert_eq!(
        b.receipt("c2").await["reason"],
        "1 of 1 wall tiles are already lent to visitors; try again when one returns"
    );
    a.intent("c3", "claim_gpu", json!({})).await;
    assert_eq!(
        a.receipt("c3").await["reason"],
        "one GPU per client at a time"
    );

    let first = a.message("claim").await["claim"].clone();
    assert_eq!(first["phase"], "restoring", "{first}");
    assert_eq!(first["frame_url"], Value::Null);
    assert_eq!(first["tile"], "fcvm:2");
    agent.report("slot-2", "running", 2);
    let claim = a
        .message_where("claim", |m| m["claim"]["phase"] == "running")
        .await["claim"]
        .clone();
    assert_eq!(claim["id"], "claim-1");
    assert_eq!(claim["frame_url"], "ws://gpu-host.example:20002/frames");
    assert_eq!(claim["gpu"], "Tesla T4");
    assert_eq!(claim["phases"]["gpu_ms"], 3000);
    let token = claim["token"].as_str().unwrap().to_string();
    assert!(token.len() >= 16);
    assert!(
        claim["first_frame_at"].as_u64().unwrap() >= claim["clicked_at"].as_u64().unwrap(),
        "{claim}"
    );

    let s = b.until(|s| s["claims"][0]["first_frame_at"].is_u64()).await;
    assert_eq!(s["tiles"][1]["phase"], "borrowed");
    assert_eq!(s["tiles"][1]["frame_url"], Value::Null);
    assert_eq!(s["tiles"][1]["borrowed"]["session"], claim["id"]);
    assert_eq!(s["borrow"]["lent"], 1);
    assert_eq!(s["claims"][0]["borrowed"], "fcvm:2");
    let all_b = format!("{:?}{:?}{:?}", b.snapshots, b.other, b.receipts);
    assert!(
        !all_b.contains(&token),
        "another viewer saw the session token"
    );
    assert!(
        !b.other.iter().any(|m| m["type"] == "claim"),
        "another viewer got the claim message"
    );

    // Releasing ends the session for its viewer, and the GPU returns to the wall.
    a.intent("rel", "release_gpu", json!({"claim": "claim-1"}))
        .await;
    assert_eq!(a.receipt("rel").await["outcome"], "committed");
    let ended = a
        .message_where("claim", |m| m["claim"]["phase"] == "ended")
        .await["claim"]
        .clone();
    assert_eq!(ended["ended"], "released");
    let slots = loop {
        let slots = agent.assign().await;
        if slots[1].private_camera.is_none() {
            break slots;
        }
    };
    assert_eq!(slots[1].generation, 3);
    assert!(slots[1].rect.is_some());
    let s = b.until(|s| s["claims"] == json!([])).await;
    assert_eq!(s["tiles"][1]["last_relaunch"]["by"], "return:claim-1");
}

/// A fake aggregator: the given picture, as raw FCVF frames on the scene clock.
async fn fake_aggregate(epoch: u64, x0: u32, yuv: [u8; 3]) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/frames", l.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = l.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(tcp).await else {
                    return;
                };
                let h = json!({"codec": "raw", "width": 4, "height": 2, "full_width": 8,
                    "full_height": 2, "x0": x0, "y0": 0, "fps": 30});
                if ws.send(Message::Text(h.to_string().into())).await.is_err() {
                    return;
                }
                let mut au = Vec::new();
                for v in yuv {
                    au.extend(std::iter::repeat_n(v, 8));
                }
                for number in 0u32.. {
                    let t = (now_ms() - epoch) as f64 / 1000.0;
                    let f = Frame {
                        key: true,
                        width: 4,
                        height: 2,
                        t,
                        number,
                        au: au.clone(),
                    };
                    if ws.send(Message::Binary(f.encode().into())).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(33)).await;
                }
            });
        }
    });
    url
}

/// At a camera hold the coordinator takes the whole image from every aggregator (a host's
/// band and the EC2 half), places each region and serves it at `/poster.jpg`, briefly
/// cacheable. Before the first hold has converged there is no poster.
#[tokio::test]
async fn the_poster_is_the_whole_image_at_a_hold() {
    let epoch = now_ms();
    let red = [63, 102, 240];
    let blue = [32, 240, 118];
    let host_agg = fake_aggregate(epoch, 0, red).await;
    let ec2_agg = fake_aggregate(epoch, 4, blue).await;
    let (handle, _actor) = start(
        State::new(Config::default(), epoch),
        None,
        None,
        Some(ec2_agg),
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let poster = Arc::new(poster::Poster::default());
    tokio::spawn(poster::run(
        handle.clone(),
        poster.clone(),
        Arc::new(Raw),
        poster::StillEncoder::Ppm,
    ));
    let cfg = live::LiveConfig {
        checker: allow_all(),
        allowed_origins: vec![],
        verifier: Arc::new(turnstile::Off),
        trust_cf_connecting_ip: false,
        turnstile_site_key: None,
        poster,
    };
    let h = handle.clone();
    tokio::spawn(async move { live::serve(listener, h, cfg).await.unwrap() });
    let agent = FakeAgent::connect(&handle, "gpu-a", 1);
    agent
        .to_coord
        .send(AgentMsg::Aggregate {
            frame_url: host_agg,
        })
        .unwrap();

    let http = reqwest::Client::new();
    let r = http.get(format!("{base}/poster.jpg")).send().await.unwrap();
    assert_eq!(r.status(), 404, "no poster before the first hold converged");
    assert_eq!(r.headers()["cache-control"], "no-store");
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let r = loop {
        let r = http.get(format!("{base}/poster.jpg")).send().await.unwrap();
        if r.status() == 200 {
            break r;
        }
        assert!(std::time::Instant::now() < deadline, "no poster after 15 s");
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let taken_t = (now_ms() - epoch) as f64 / 1000.0;
    assert!(
        taken_t >= poster::CONVERGE_SECS,
        "the poster was taken before the hold converged (t={taken_t})"
    );
    assert_eq!(r.headers()["cache-control"], "public, max-age=10");
    assert_eq!(
        r.headers()["access-control-allow-origin"],
        "*",
        "the page reads the poster from its own origin"
    );
    assert_eq!(r.headers()["content-type"], "image/x-portable-pixmap");
    let body = r.bytes().await.unwrap();
    let header = b"P6\n8 2\n255\n";
    assert!(
        body.starts_with(header),
        "{:?}",
        &body[..12.min(body.len())]
    );
    let px = |x: usize, y: usize| {
        let i = header.len() + (y * 8 + x) * 3;
        [body[i], body[i + 1], body[i + 2]]
    };
    let near = |got: [u8; 3], want: [u8; 3]| got.iter().zip(want).all(|(g, w)| g.abs_diff(w) <= 3);
    for y in 0..2 {
        assert!(near(px(0, y), [255, 0, 0]), "left {:?}", px(0, y));
        assert!(near(px(3, y), [255, 0, 0]), "left {:?}", px(3, y));
        assert!(near(px(4, y), [0, 0, 255]), "right {:?}", px(4, y));
        assert!(near(px(7, y), [0, 0, 255]), "right {:?}", px(7, y));
    }
    drop(agent);
}

/// A cross-origin browser is refused unless its origin is configured; same-origin pages
/// and clients that send no Origin (not browsers) are served.
#[tokio::test]
async fn live_refuses_origins_it_was_not_given() {
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
        State::new(
            Config {
                light_interval_ms: 0,
                ..Config::default()
            },
            1_000,
        ),
        Some(store::spawn_writer(path.clone())),
        None,
        None,
        None,
    );
    let mut last = 0;
    for i in 0..5 {
        let r = handle
            .apply(Event::Intent {
                at: 0,
                viewer: "v-1".into(),
                client: Some("198.51.100.9".into()),
                id: format!("l-{i}"),
                name: "light".into(),
                args: light(i as f64),
                check: None,
                secret: None,
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
    assert!(saved.receipts.contains_key("l-4"));
    tokio::time::timeout(Duration::from_secs(5), actor)
        .await
        .unwrap()
        .unwrap();
}

/// EC2 refuses capacity for the race's first three launches; the tile records how long it
/// waited, and the EC2 half's race time leaves that wait out.
struct ScarceEc2 {
    calls: std::sync::atomic::AtomicU64,
}

#[async_trait::async_trait]
impl ec2::Ec2Driver for ScarceEc2 {
    async fn launch(&self, _tile: &str) -> Result<ec2::Launched, ec2::LaunchError> {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if (1..=3).contains(&n) {
            return Err(ec2::LaunchError::NoCapacity("no spot capacity".into()));
        }
        Ok(ec2::Launched {
            instance: format!("i-test-{n}"),
            frame_url: None,
        })
    }
    async fn ready(&self, _instance: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn terminate(&self, _instance: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn capacity_retry(&self) -> Duration {
        Duration::from_millis(300)
    }
}

#[tokio::test]
async fn an_ec2_capacity_wait_is_kept_out_of_the_race_time() {
    let config = Config {
        ec2_max: 1,
        max_tiles: 1,
        restart_cooldown_secs: 0,
        ..Config::default()
    };
    let driver: Arc<dyn ec2::Ec2Driver> = Arc::new(ScarceEc2 {
        calls: Default::default(),
    });
    let (handle, _actor) = start(State::new(config, now_ms()), None, Some(driver), None, None);
    let mut agent = FakeAgent::connect(&handle, "gpu-a", 1);
    agent.assign().await;
    agent.report("slot-1", "running", 1);
    let snaps = handle.snapshot.clone();
    let state = |s: &Arc<fcvm::fleet::coord::Snapshot>| -> Value {
        serde_json::from_str::<Value>(&s.text).unwrap()["state"].clone()
    };
    let wait = |f: Box<dyn Fn(&Value) -> bool>| {
        let mut snaps = snaps.clone();
        async move {
            tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    let s = state(&snaps.borrow_and_update());
                    if f(&s) {
                        return s;
                    }
                    snaps.changed().await.unwrap();
                }
            })
            .await
            .expect("condition within 20 s")
        }
    };
    wait(Box::new(|s| s["tiles"][1]["phase"] == "running")).await;
    let r = handle
        .apply(Event::Intent {
            at: 0,
            viewer: "v-1".into(),
            client: Some("198.51.100.9".into()),
            id: "race".into(),
            name: "restart_now".into(),
            args: json!({}),
            check: None,
            secret: None,
        })
        .await
        .unwrap();
    assert_eq!(r.outcome, fleet_core::state::Outcome::Committed);
    agent.report("slot-1", "running", 2);
    let s = wait(Box::new(|s| s["races"][0]["ec2"]["done_at"].is_u64())).await;
    let race = &s["races"][0];
    let h = &race["ec2"];
    let wait_ms = h["capacity_wait_ms"].as_u64().unwrap();
    assert!(
        wait_ms >= 900,
        "three 300 ms capacity retries, got {wait_ms} ms"
    );
    let wall = h["done_at"].as_u64().unwrap() - race["started_at"].as_u64().unwrap();
    let time = h["time_ms"].as_u64().unwrap();
    assert!(
        time + wait_ms <= wall + 5 && time < 900,
        "the race time {time} ms must leave out the {wait_ms} ms wait (click to frame {wall} ms)"
    );
    assert_eq!(s["scoreboard"]["ec2"]["last_capacity_wait_ms"], wait_ms);
}

/// A GPU the agent quarantines leaves the wall (a standby GPU takes its place) and comes
/// back only when an operator releases it through the admin endpoint. Restores count in
/// the snapshot.
#[tokio::test]
async fn a_quarantined_gpu_returns_only_by_the_operator() {
    let config = Config {
        max_tiles: 1,
        ..Config::default()
    };
    let (handle, _actor) = start(State::new(config, now_ms()), None, None, None, None);
    let admin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let admin_url = format!("http://{}", admin.local_addr().unwrap());
    let h = handle.clone();
    tokio::spawn(async move {
        axum::serve(admin, fcvm::fleet::coord::admin_router(h))
            .await
            .unwrap()
    });
    let mut agent = FakeAgent::connect(&handle, "gpu-a", 2);
    let slots = agent.assign().await;
    assert!(slots[1].standby && slots[1].rect.is_none(), "{slots:?}");
    assert!(!slots[0].standby && slots[0].rect.is_some());
    agent.report("slot-1", "running", 1);
    agent
        .to_coord
        .send(AgentMsg::Quarantine {
            slot: "slot-1".into(),
            sbdf: "0000:01:00.0".into(),
            error: "GPU 0000:01:00.0 did not reset".into(),
        })
        .unwrap();
    let slots = loop {
        let slots = agent.assign().await;
        if slots.len() == 1 {
            break slots;
        }
    };
    assert_eq!(slots[0].id, "slot-2");
    assert!(!slots[0].standby && slots[0].rect.is_some(), "{slots:?}");
    let snap = |h: &Handle| -> Value {
        serde_json::from_str::<Value>(&h.snapshot.borrow().text).unwrap()["state"].clone()
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while snap(&handle)["failures"][0]["kind"] != "gpu_quarantined" {
        assert!(std::time::Instant::now() < deadline, "{}", snap(&handle));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(snap(&handle)["counters"]["restores"], 1);
    let r = reqwest::Client::new()
        .post(format!("{admin_url}/quarantine/release"))
        .json(&json!({"host": "gpu-a", "sbdf": "0000:01:00.0"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let slots = loop {
        let slots = agent.assign().await;
        if slots.len() == 2 {
            break slots;
        }
    };
    assert_eq!(slots[0].id, "slot-1");
    assert!(slots[0].rect.is_some() && slots[1].standby, "{slots:?}");
}

/// With scale groups, the fleet scales them to zero after the idle time with no active
/// viewer (a hidden page does not count), and back up when a viewer's page is visible
/// again; while it wakes, the groups' instances show in the snapshot.
#[tokio::test]
async fn no_active_viewer_scales_the_gpus_to_zero_and_one_wakes_them() {
    use fcvm::fleet::coord::scale::{FakeScale, Group, PowerConfig};
    let config = Config {
        power_managed: true,
        idle_scale_down_secs: 1,
        ..Config::default()
    };
    let fake = Arc::new(FakeScale::default());
    *fake.instances.lock().unwrap() = vec![fleet_core::event::ScaleInstance {
        id: "i-0123456789abcdef0".into(),
        lifecycle: "Pending".into(),
    }];
    let power = PowerConfig {
        driver: fake.clone(),
        groups: vec![Group {
            region: "us-east-1".into(),
            name: "fleet-hosts".into(),
        }],
        awake_capacity: 2,
        poll: Duration::from_millis(100),
    };
    let (handle, _actor) = start(State::new(config, now_ms()), None, None, None, Some(power));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/live", listener.local_addr().unwrap());
    let cfg = live::LiveConfig {
        checker: allow_all(),
        allowed_origins: vec![],
        verifier: Arc::new(turnstile::Off),
        trust_cf_connecting_ip: false,
        turnstile_site_key: None,
        poster: Arc::new(poster::Poster::default()),
    };
    let h = handle.clone();
    tokio::spawn(async move { live::serve(listener, h, cfg).await.unwrap() });
    let calls = || fake.calls.lock().unwrap().clone();
    let mut v = Viewer::connect(&url).await;
    v.until(|s| s["viewers"]["active"] == 1).await;
    assert_eq!(
        calls(),
        vec![("fleet-hosts".to_string(), 2)],
        "awake at start"
    );
    v.send(json!({"type": "visibility", "visible": false}))
        .await;
    let s = v.until(|s| s["power"]["state"] == "asleep").await;
    assert_eq!(s["viewers"]["active"], 0);
    assert_eq!(s["viewers"]["count"], 1);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while calls().len() < 2 {
        assert!(std::time::Instant::now() < deadline, "{:?}", calls());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(calls()[1], ("fleet-hosts".to_string(), 0));
    v.send(json!({"type": "visibility", "visible": true})).await;
    let s = v
        .until(|s| s["power"]["state"] == "waking" && s["power"]["hosts"][0]["phase"] == "pending")
        .await;
    assert_eq!(s["power"]["hosts"][0]["instance"], "i-0123456789abcdef0");
    assert!(s["power"]["woke_at"].is_u64());
    assert_eq!(calls()[2], ("fleet-hosts".to_string(), 2));
}
