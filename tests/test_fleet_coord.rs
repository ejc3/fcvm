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
use std::time::{Duration, Instant};
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
    /// A DNS writer: tiles' and claims' frame URLs are then the published names.
    dns: Option<fcvm::fleet::coord::dns::Cloudflare>,
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
            dns: None,
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
        o.dns,
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
        // The two viewers' sockets are read by separate tasks, so without this wait the
        // next viewer's light can be committed first and the scene's order below is the
        // order of arrival, not of sending.
        who.receipt(&id).await;
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
                store_ready_at: None,
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
                ipv6: None,
                ipv4: None,
                v4_port: None,
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

    // A second connection naming the same host (an agent that dialed again while the old
    // connection is still held) replaces the first: the host keeps its slots at their
    // generations, so no VM is relaunched.
    let mut again = FakeAgent::connect(&handle, "gpu-a", 2);
    let slots = again.assign().await;
    assert_eq!((slots[0].generation, slots[1].generation), (3, 2));
    // The old connection closing afterwards takes nothing away.
    drop(agent.to_coord);
    agent.conn.await.unwrap().unwrap();
    v.intent("l2", "light", light(6.0)).await;
    let r = v.receipt("l2").await;
    let s = v.at_least(r["version"].as_u64().unwrap()).await;
    assert_eq!(
        s["fleet"]["n"], 2,
        "the replaced connection's close left the host"
    );

    // The agent going away takes its slots out of the tiling.
    drop(again.to_coord);
    again.conn.await.unwrap().unwrap();
    loop {
        let (_, s) = v.latest();
        if s["fleet"]["n"] == 0 {
            assert_eq!(s["tiles"], json!([]));
            break;
        }
        v.recv().await;
    }
}

/// With published names, a claim's frame URLs are its launch's published names, like a
/// tile's: the agent's own URL names a host that has no record.
#[tokio::test]
async fn a_claims_frame_urls_are_its_launchs_published_names() {
    use fcvm::fleet::coord::dns::Cloudflare;
    let cf = FakeCf::default();
    let base = fake_cloudflare(cf.clone()).await;
    let gate = GateConfirm {
        open: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        cf: cf.clone(),
        asked_before_written: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let writer = Cloudflare::with_endpoint(
        "dns-token-for-tests".into(),
        "fcvm.example".into(),
        Some("zone-for-tests".into()),
        &base,
    )
    .unwrap()
    .with_confirm(Arc::new(gate));
    let c = coordinator_with(Opts {
        dns: Some(writer),
        ..Opts::default()
    })
    .await;
    let mut agent = FakeAgent::connect(&c.handle, "gpu-a", 1);
    agent.assign().await;
    let report = |generation: u64, v6: &str| {
        AgentMsg::Slot(SlotReport {
            id: "slot-1".into(),
            phase: "running".into(),
            generation,
            frame_url: Some("wss://slot-1.fcvm.example/frames".into()),
            ipv6: Some(v6.into()),
            ipv4: Some("192.0.2.10".into()),
            v4_port: Some(20200),
            ..Default::default()
        })
    };
    agent.to_coord.send(report(1, "2001:db8::1")).unwrap();
    let mut a = Viewer::connect_from(&c.url, Some("198.51.100.1")).await;
    a.until(|s| !s["tiles"][0]["frame_url"].is_null()).await;
    a.intent("c1", "claim_gpu", json!({})).await;
    assert_eq!(a.receipt("c1").await["outcome"], "committed");
    agent.assign().await;
    agent.to_coord.send(report(2, "2001:db8::1")).unwrap();
    let claim = a
        .message_where("claim", |m| {
            m["claim"]["phase"] == "running" && !m["claim"]["frame_url"].is_null()
        })
        .await["claim"]
        .clone();
    let url = claim["frame_url"].as_str().unwrap();
    let host = url
        .strip_prefix("wss://")
        .and_then(|u| u.strip_suffix("/frames"))
        .unwrap();
    assert!(
        host.starts_with("slot-1-") && host.ends_with(".fcvm.example"),
        "{claim}"
    );
    assert!(
        cf.records
            .lock()
            .unwrap()
            .values()
            .any(|r| r["name"] == host),
        "the claim's URL names no record: {claim}"
    );
    assert_eq!(
        claim["frame_url_v4"],
        format!(
            "wss://{}-v4.fcvm.example:20200/frames",
            host.strip_suffix(".fcvm.example").unwrap()
        )
    );
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
    // One EC2 tile, so the EC2 side has its half of the image and its aggregator a region.
    let config = Config {
        ec2_max: 1,
        ..Config::default()
    };
    let ec2: Arc<dyn ec2::Ec2Driver> = Arc::new(ec2::FakeEc2::new(
        Duration::from_millis(10),
        Duration::from_millis(10),
    ));
    let (handle, _actor) = start(
        State::new(config, epoch),
        None,
        Some(ec2),
        Some(ec2_agg),
        None,
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
    agent.report("slot-1", "running", 1);

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

/// A tile's instance that goes away after it was ready (a spot reclaim) is reported
/// failed and launched again; without that the tile stays "running" on a dead instance
/// until someone relaunches it.
struct ReclaimedEc2 {
    launches: std::sync::atomic::AtomicU64,
    terminated: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl ec2::Ec2Driver for ReclaimedEc2 {
    async fn launch(&self, _tile: &str) -> Result<ec2::Launched, ec2::LaunchError> {
        let n = self
            .launches
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ec2::Launched {
            instance: format!("i-test-{n}"),
            frame_url: None,
            ipv6: None,
            ipv4: None,
        })
    }
    async fn ready(&self, _tile: &str, l: &ec2::Launched) -> anyhow::Result<ec2::Launched> {
        Ok(l.clone())
    }
    async fn terminate(&self, instance: &str) -> anyhow::Result<()> {
        self.terminated.lock().unwrap().push(instance.to_string());
        Ok(())
    }
    async fn post_scene(
        &self,
        _tile: &str,
        _l: &ec2::Launched,
        _body: &fleet_core::protocol::RendererScene,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn post_budget(
        &self,
        _tile: &str,
        _l: &ec2::Launched,
        _body: &fleet_core::protocol::BudgetBody,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn health(&self, _tile: &str, _l: &ec2::Launched) -> anyhow::Result<serde_json::Value> {
        anyhow::bail!("no renderer")
    }
    async fn alive(&self, _tile: &str, l: &ec2::Launched) -> anyhow::Result<ec2::Liveness> {
        // The first instance is reclaimed once it has run.
        Ok(if l.instance == "i-test-0" {
            ec2::Liveness::Dead("instance i-test-0 is terminated".into())
        } else {
            ec2::Liveness::Alive
        })
    }
    fn liveness_every(&self) -> Duration {
        Duration::from_millis(100)
    }
}

#[tokio::test]
async fn an_ec2_tile_whose_instance_is_reclaimed_is_launched_again() {
    let config = Config {
        ec2_max: 1,
        max_tiles: 1,
        ..Config::default()
    };
    let terminated = Arc::new(std::sync::Mutex::new(Vec::new()));
    let driver: Arc<dyn ec2::Ec2Driver> = Arc::new(ReclaimedEc2 {
        launches: Default::default(),
        terminated: terminated.clone(),
    });
    let (handle, _actor) = start(
        State::new(config, now_ms()),
        None,
        Some(driver),
        None,
        None,
        None,
    );
    let mut agent = FakeAgent::connect(&handle, "gpu-a", 1);
    agent.assign().await;
    agent.report("slot-1", "running", 1);
    let mut snaps = handle.snapshot.clone();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let v: Value = serde_json::from_str(&snaps.borrow_and_update().text).unwrap();
            let t = &v["state"]["tiles"][1];
            if t["phase"] == "running" && t["vm"] == "i-test-1" {
                return;
            }
            snaps.changed().await.unwrap();
        }
    })
    .await
    .expect("the tile runs on a new instance after its first was reclaimed");
    assert_eq!(terminated.lock().unwrap()[0], "i-test-0");
}

/// A tile that leaves the assignment and comes back gets a new task while the old one can
/// still be waiting for its instance to be ready. The old task must neither overwrite nor
/// remove the new task's running instance: without that the new instance gets no scene,
/// and the tile stays black until its next relaunch.
struct RetiredEc2 {
    launches: std::sync::atomic::AtomicU64,
    // Opened to let the first instance become ready, and told when its wait began.
    gate: Arc<tokio::sync::Notify>,
    waiting: Arc<tokio::sync::Notify>,
    scenes: Arc<std::sync::Mutex<Vec<(String, i64)>>>,
    terminated: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl ec2::Ec2Driver for RetiredEc2 {
    async fn launch(&self, _tile: &str) -> Result<ec2::Launched, ec2::LaunchError> {
        let n = self
            .launches
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ec2::Launched {
            instance: format!("i-test-{n}"),
            frame_url: None,
            ipv6: None,
            ipv4: None,
        })
    }
    async fn ready(&self, _tile: &str, l: &ec2::Launched) -> anyhow::Result<ec2::Launched> {
        if l.instance == "i-test-0" {
            self.waiting.notify_one();
            self.gate.notified().await;
        }
        Ok(l.clone())
    }
    async fn terminate(&self, instance: &str) -> anyhow::Result<()> {
        self.terminated.lock().unwrap().push(instance.to_string());
        Ok(())
    }
    async fn post_scene(
        &self,
        _tile: &str,
        l: &ec2::Launched,
        body: &fleet_core::protocol::RendererScene,
    ) -> anyhow::Result<()> {
        let epoch = serde_json::to_value(body).unwrap()["epoch_unix_ms"]
            .as_i64()
            .unwrap_or(-1);
        self.scenes
            .lock()
            .unwrap()
            .push((l.instance.clone(), epoch));
        Ok(())
    }
    async fn post_budget(
        &self,
        _tile: &str,
        _l: &ec2::Launched,
        _body: &fleet_core::protocol::BudgetBody,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn health(&self, _tile: &str, _l: &ec2::Launched) -> anyhow::Result<serde_json::Value> {
        anyhow::bail!("no renderer")
    }
}

#[tokio::test]
async fn a_retired_tile_task_leaves_its_replacement_running() {
    let (handle, _actor) = start(
        State::new(Config::default(), now_ms()),
        None,
        None,
        None,
        None,
        None,
    );
    let gate = Arc::new(tokio::sync::Notify::new());
    let waiting = Arc::new(tokio::sync::Notify::new());
    let scenes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let terminated = Arc::new(std::sync::Mutex::new(Vec::new()));
    let driver: Arc<dyn ec2::Ec2Driver> = Arc::new(RetiredEc2 {
        launches: Default::default(),
        gate: gate.clone(),
        waiting: waiting.clone(),
        scenes: scenes.clone(),
        terminated: terminated.clone(),
    });
    let slot: fleet_core::protocol::AssignSlot = serde_json::from_value(json!({
        "id": "ec2-1", "sbdf": "", "generation": 1,
        "rect": {"x": 0.5, "y": 0.0, "w": 0.5, "h": 1.0}
    }))
    .unwrap();
    let scene = |epoch: u64| {
        Some(fleet_core::protocol::SharedScene::new(
            &fleet_core::scene::Scene::new(epoch),
            16.0 / 9.0,
        ))
    };
    let (assign_tx, assign_rx) = tokio::sync::watch::channel(vec![slot.clone()]);
    let (scene_tx, scene_rx) = tokio::sync::watch::channel(scene(1_000));
    let (_budget_tx, budget_rx) = tokio::sync::watch::channel(None);
    tokio::spawn(ec2::run(
        driver,
        assign_rx,
        scene_rx,
        budget_rx,
        handle.clone(),
    ));
    // The first task waits for its instance (i-test-0); the tile leaves and comes back.
    tokio::time::timeout(Duration::from_secs(5), waiting.notified())
        .await
        .expect("the first instance is waited for");
    assign_tx.send(Vec::new()).unwrap();
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assign_tx.send(vec![slot]).unwrap();
    let posted = |instance: &str, epoch: i64| {
        let scenes = scenes.clone();
        let instance = instance.to_string();
        async move {
            loop {
                if scenes
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(i, e)| *i == instance && *e == epoch)
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), posted("i-test-1", 1_000))
        .await
        .expect("the returning tile's instance gets the scene");
    // Now the first instance becomes ready, after its task was retired.
    gate.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !terminated.lock().unwrap().contains(&"i-test-0".to_string()) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the retired task terminates its instance");
    // A new scene still reaches the running instance, and never the retired one.
    scene_tx.send(scene(2_000)).unwrap();
    tokio::time::timeout(Duration::from_secs(5), posted("i-test-1", 2_000))
        .await
        .expect("the replacement is still the tile's running instance");
    assert!(
        !scenes.lock().unwrap().iter().any(|(i, _)| i == "i-test-0"),
        "{:?}",
        scenes.lock().unwrap()
    );
}

/// A handle whose actor only records the EC2 tiles' reports, in order.
fn recording_handle() -> (Handle, Arc<std::sync::Mutex<Vec<SlotReport>>>) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<fcvm::fleet::coord::Cmd>(256);
    let reports = Arc::new(std::sync::Mutex::new(Vec::new()));
    let r2 = reports.clone();
    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            if let fcvm::fleet::coord::Cmd::Apply { event, reply } = cmd {
                if let Event::Ec2 { report, .. } = event {
                    r2.lock().unwrap().push(report);
                }
                if let Some(reply) = reply {
                    let _ = reply.send(None);
                }
            }
        }
    });
    let snapshot = tokio::sync::watch::channel(Arc::new(fcvm::fleet::coord::Snapshot {
        version: 0,
        text: String::new(),
    }))
    .1;
    let ec2_layout = tokio::sync::watch::channel(None).1;
    (
        Handle {
            cmds: tx,
            snapshot,
            ec2_layout,
        },
        reports,
    )
}

/// The first instance of a tile that left and came back fails its readiness check after
/// the replacement runs. The retired task terminates it and reports nothing: a `failed`
/// report on the slot would replace the replacement's `running` and clear its addresses
/// until the next relaunch.
struct RetiredFailsEc2 {
    launches: std::sync::atomic::AtomicU64,
    gate: Arc<tokio::sync::Notify>,
    waiting: Arc<tokio::sync::Notify>,
    terminated: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl ec2::Ec2Driver for RetiredFailsEc2 {
    async fn launch(&self, _tile: &str) -> Result<ec2::Launched, ec2::LaunchError> {
        let n = self
            .launches
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ec2::Launched {
            instance: format!("i-test-{n}"),
            frame_url: None,
            ipv6: None,
            ipv4: None,
        })
    }
    async fn ready(&self, _tile: &str, l: &ec2::Launched) -> anyhow::Result<ec2::Launched> {
        if l.instance == "i-test-0" {
            self.waiting.notify_one();
            self.gate.notified().await;
            anyhow::bail!("i-test-0 did not serve /health");
        }
        Ok(ec2::Launched {
            ipv4: Some("192.0.2.10".into()),
            ..l.clone()
        })
    }
    async fn terminate(&self, instance: &str) -> anyhow::Result<()> {
        self.terminated.lock().unwrap().push(instance.to_string());
        Ok(())
    }
    async fn post_scene(
        &self,
        _tile: &str,
        _l: &ec2::Launched,
        _body: &fleet_core::protocol::RendererScene,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn post_budget(
        &self,
        _tile: &str,
        _l: &ec2::Launched,
        _body: &fleet_core::protocol::BudgetBody,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn health(&self, _tile: &str, _l: &ec2::Launched) -> anyhow::Result<serde_json::Value> {
        anyhow::bail!("no renderer")
    }
}

#[tokio::test]
async fn a_retired_tile_task_whose_instance_fails_reports_nothing_on_the_replacement() {
    let (handle, reports) = recording_handle();
    let gate = Arc::new(tokio::sync::Notify::new());
    let waiting = Arc::new(tokio::sync::Notify::new());
    let terminated = Arc::new(std::sync::Mutex::new(Vec::new()));
    let driver: Arc<dyn ec2::Ec2Driver> = Arc::new(RetiredFailsEc2 {
        launches: Default::default(),
        gate: gate.clone(),
        waiting: waiting.clone(),
        terminated: terminated.clone(),
    });
    let slot: fleet_core::protocol::AssignSlot = serde_json::from_value(json!({
        "id": "ec2-1", "sbdf": "", "generation": 1,
        "rect": {"x": 0.5, "y": 0.0, "w": 0.5, "h": 1.0}
    }))
    .unwrap();
    let (assign_tx, assign_rx) = tokio::sync::watch::channel(vec![slot.clone()]);
    let (_scene_tx, scene_rx) = tokio::sync::watch::channel(None);
    let (_budget_tx, budget_rx) = tokio::sync::watch::channel(None);
    tokio::spawn(ec2::run(driver, assign_rx, scene_rx, budget_rx, handle));
    tokio::time::timeout(Duration::from_secs(5), waiting.notified())
        .await
        .expect("the first instance is waited for");
    assign_tx.send(Vec::new()).unwrap();
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assign_tx.send(vec![slot]).unwrap();
    let running = |reports: &[SlotReport]| {
        reports
            .iter()
            .any(|r| r.phase == "running" && r.vm.as_deref() == Some("i-test-1"))
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        while !running(&reports.lock().unwrap()) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the replacement runs");
    gate.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !terminated.lock().unwrap().contains(&"i-test-0".to_string()) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the retired task terminates its instance");
    // The retired task reports before it terminates, so its report is in by now.
    let reports = reports.lock().unwrap();
    let last = reports.last().unwrap();
    assert!(
        last.phase == "running" && last.vm.as_deref() == Some("i-test-1"),
        "the slot's last report is not the replacement's: {reports:#?}"
    );
    assert!(!reports.iter().any(|r| r.phase == "failed"), "{reports:#?}");
}

/// A running tile whose renderer stops answering on an instance that keeps running is
/// launched again after three silent checks in a row; a check it answers starts the count
/// again. Before, only the instance's state was checked and the tile stayed `running`.
struct SilentRendererEc2 {
    launches: std::sync::atomic::AtomicU64,
    /// The first instance's renderer: what each check finds, then silence from then on.
    first: std::sync::Mutex<std::collections::VecDeque<ec2::Liveness>>,
    checks: Arc<std::sync::atomic::AtomicUsize>,
    terminated: Arc<std::sync::Mutex<Vec<(String, usize)>>>,
}

#[async_trait::async_trait]
impl ec2::Ec2Driver for SilentRendererEc2 {
    async fn launch(&self, _tile: &str) -> Result<ec2::Launched, ec2::LaunchError> {
        let n = self
            .launches
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ec2::Launched {
            instance: format!("i-test-{n}"),
            frame_url: None,
            ipv6: None,
            ipv4: None,
        })
    }
    async fn ready(&self, _tile: &str, l: &ec2::Launched) -> anyhow::Result<ec2::Launched> {
        Ok(l.clone())
    }
    async fn terminate(&self, instance: &str) -> anyhow::Result<()> {
        let checks = self.checks.load(std::sync::atomic::Ordering::SeqCst);
        self.terminated
            .lock()
            .unwrap()
            .push((instance.to_string(), checks));
        Ok(())
    }
    async fn post_scene(
        &self,
        _tile: &str,
        _l: &ec2::Launched,
        _body: &fleet_core::protocol::RendererScene,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn post_budget(
        &self,
        _tile: &str,
        _l: &ec2::Launched,
        _body: &fleet_core::protocol::BudgetBody,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn health(&self, _tile: &str, _l: &ec2::Launched) -> anyhow::Result<serde_json::Value> {
        anyhow::bail!("no renderer")
    }
    async fn alive(&self, _tile: &str, l: &ec2::Launched) -> anyhow::Result<ec2::Liveness> {
        if l.instance != "i-test-0" {
            return Ok(ec2::Liveness::Alive);
        }
        self.checks
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self
            .first
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| ec2::Liveness::Silent("connection refused".into())))
    }
    fn liveness_every(&self) -> Duration {
        Duration::from_millis(50)
    }
}

#[tokio::test]
async fn an_ec2_tile_whose_renderer_stops_answering_is_launched_again() {
    use ec2::Liveness::{Alive, Silent};
    let (handle, reports) = recording_handle();
    let checks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let terminated = Arc::new(std::sync::Mutex::new(Vec::new()));
    let silent = || Silent("connection refused".into());
    let driver: Arc<dyn ec2::Ec2Driver> = Arc::new(SilentRendererEc2 {
        launches: Default::default(),
        // Silent, answered, silent twice, answered, then silent from the sixth check on:
        // the third silent check in a row is the eighth.
        first: std::sync::Mutex::new(
            [silent(), Alive, silent(), silent(), Alive]
                .into_iter()
                .collect(),
        ),
        checks: checks.clone(),
        terminated: terminated.clone(),
    });
    let slot: fleet_core::protocol::AssignSlot = serde_json::from_value(json!({
        "id": "ec2-1", "sbdf": "", "generation": 1,
        "rect": {"x": 0.5, "y": 0.0, "w": 0.5, "h": 1.0}
    }))
    .unwrap();
    let (_assign_tx, assign_rx) = tokio::sync::watch::channel(vec![slot]);
    let (_scene_tx, scene_rx) = tokio::sync::watch::channel(None);
    let (_budget_tx, budget_rx) = tokio::sync::watch::channel(None);
    tokio::spawn(ec2::run(driver, assign_rx, scene_rx, budget_rx, handle));
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if reports
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.phase == "running" && r.vm.as_deref() == Some("i-test-1"))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the tile runs on a new instance after its renderer stopped answering");
    assert_eq!(
        terminated.lock().unwrap()[0],
        ("i-test-0".to_string(), 8),
        "relaunched after the third silent check in a row"
    );
    let reports = reports.lock().unwrap();
    let failed = reports
        .iter()
        .find(|r| r.phase == "failed")
        .expect("a failed report");
    assert!(
        failed
            .error
            .as_deref()
            .is_some_and(|e| e.contains("did not answer 3 checks") && e.contains("i-test-0")),
        "{failed:?}"
    );
}

/// The periodic store has queued the newest state and its write is still running when
/// the coordinator is stopped: shutdown must wait for that write, not report the state
/// written while it is not.
#[tokio::test]
async fn shutdown_waits_for_a_state_write_already_in_flight() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<store::StoreCmd>();
    let written = Arc::new(std::sync::Mutex::new(Vec::<usize>::new()));
    let w2 = written.clone();
    // A slow writer: each write takes 2 s, then it is recorded and acknowledged.
    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            if let Some(b) = &cmd.bytes {
                tokio::time::sleep(Duration::from_secs(2)).await;
                w2.lock().unwrap().push(b.len());
            }
            if let Some(a) = cmd.ack {
                let _ = a.send(true);
            }
        }
    });
    let (handle, actor) = start(
        State::new(
            Config {
                light_interval_ms: 0,
                ..Config::default()
            },
            1_000,
        ),
        Some(tx),
        None,
        None,
        None,
        None,
    );
    // Let the first store tick (at start, an unchanged state) pass, then change the state.
    tokio::time::sleep(Duration::from_millis(100)).await;
    handle
        .apply(Event::Intent {
            at: 0,
            viewer: "v-1".into(),
            client: Some("198.51.100.9".into()),
            id: "l-0".into(),
            name: "light".into(),
            args: light(0.0),
            check: None,
            secret: None,
        })
        .await
        .unwrap();
    // The next store tick (1 s) queues the write; stop while it is running.
    tokio::time::sleep(Duration::from_millis(1400)).await;
    assert!(
        written.lock().unwrap().is_empty(),
        "the write is still running"
    );
    handle.shutdown().await;
    assert_eq!(
        written.lock().unwrap().len(),
        1,
        "shutdown returned before the queued write finished"
    );
    tokio::time::timeout(Duration::from_secs(5), actor)
        .await
        .unwrap()
        .unwrap();
}

/// A state write that fails (here: its directory does not exist yet) is kept, and the next
/// command to the writer, a shutdown barrier that carries no state of its own, writes it.
/// Before, the writer dropped it and acknowledged the barrier with nothing written.
#[tokio::test]
async fn a_failed_state_write_is_kept_and_written_by_the_next_barrier() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("later").join("fleet.json");
    let tx = store::spawn_writer(path.clone());
    let mut s = State::new(Config::default(), 1_000);
    s.version = 7;
    let (ack, done) = tokio::sync::oneshot::channel();
    tx.send(store::StoreCmd {
        bytes: Some(Arc::new(serde_json::to_vec(&s).unwrap())),
        ack: Some(ack),
    })
    .unwrap();
    let first = done.await.unwrap();
    assert!(!first, "a failed write is acknowledged as not stored");
    assert!(store::load(&path).unwrap().is_none());
    std::fs::create_dir(dir.path().join("later")).unwrap();
    let (ack, done) = tokio::sync::oneshot::channel();
    tx.send(store::StoreCmd {
        bytes: None,
        ack: Some(ack),
    })
    .unwrap();
    let second = done.await.unwrap();
    let saved = store::load(&path)
        .unwrap()
        .expect("the barrier wrote the state that failed before");
    assert_eq!(saved.version, 7);
    assert!(second, "the retried write is acknowledged as stored");
}

/// A writer whose writes fail: shutdown asks again, and reports that the last state is not
/// stored rather than counting it stored. A write that succeeds on a later attempt is
/// reported stored.
#[tokio::test]
async fn shutdown_reports_a_last_state_that_was_not_stored() {
    async fn run(fail_first: usize) -> (bool, usize) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<store::StoreCmd>();
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let a2 = attempts.clone();
        tokio::spawn(async move {
            let mut pending = false;
            while let Some(cmd) = rx.recv().await {
                pending |= cmd.bytes.is_some();
                if pending {
                    let n = a2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    pending = n < fail_first;
                }
                if let Some(a) = cmd.ack {
                    let _ = a.send(!pending);
                }
            }
        });
        let (handle, actor) = start(
            State::new(
                Config {
                    light_interval_ms: 0,
                    ..Config::default()
                },
                1_000,
            ),
            Some(tx),
            None,
            None,
            None,
            None,
        );
        handle
            .apply(Event::Intent {
                at: 0,
                viewer: "v-1".into(),
                client: Some("198.51.100.9".into()),
                id: "l-0".into(),
                name: "light".into(),
                args: light(0.0),
                check: None,
                secret: None,
            })
            .await
            .unwrap();
        let stored = handle.shutdown().await;
        tokio::time::timeout(Duration::from_secs(10), actor)
            .await
            .unwrap()
            .unwrap();
        (stored, attempts.load(std::sync::atomic::Ordering::SeqCst))
    }
    assert_eq!(run(usize::MAX).await, (false, 3));
    assert_eq!(run(1).await, (true, 2));
}

/// A state write fails and the state then stays unchanged: the periodic store asks the
/// writer again on its next tick instead of leaving the state unwritten until the next
/// change or shutdown.
#[tokio::test]
async fn a_failed_state_write_is_retried_while_the_state_is_unchanged() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<store::StoreCmd>();
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let a2 = attempts.clone();
    // Fails its first write and stores every later one.
    tokio::spawn(async move {
        let mut pending = false;
        while let Some(cmd) = rx.recv().await {
            pending |= cmd.bytes.is_some();
            if pending {
                let n = a2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                pending = n == 0;
            }
            if let Some(a) = cmd.ack {
                let _ = a.send(!pending);
            }
        }
    });
    // The actor alone, on a clock that stands still and with no tick task, so nothing
    // changes the state after the intent below.
    let (snapshot, snapshot_rx) =
        tokio::sync::watch::channel(Arc::new(fcvm::fleet::coord::Snapshot {
            version: 0,
            text: String::new(),
        }));
    let (ec2_layout, ec2_layout_rx) = tokio::sync::watch::channel(None);
    let state = State::new(
        Config {
            light_interval_ms: 0,
            ..Config::default()
        },
        1_000,
    );
    let outputs = fcvm::fleet::coord::Outputs {
        snapshot,
        ec2: tokio::sync::watch::channel(Vec::new()).0,
        ec2_layout,
        power: tokio::sync::watch::channel(String::new()).0,
        scene: tokio::sync::watch::channel(None).0,
        budget: tokio::sync::watch::channel(None).0,
        dns: None,
        store: Some(tx),
    };
    let (cmds, cmds_rx) = tokio::sync::mpsc::channel(64);
    let actor = tokio::spawn(
        fcvm::fleet::coord::Actor::new(state, outputs, Box::new(|| 2_000)).run(cmds_rx),
    );
    let handle = Handle {
        cmds,
        snapshot: snapshot_rx,
        ec2_layout: ec2_layout_rx,
    };
    let before = handle.snapshot.borrow().version;
    handle
        .apply(Event::Intent {
            at: 0,
            viewer: "v-1".into(),
            client: Some("198.51.100.9".into()),
            id: "l-0".into(),
            name: "light".into(),
            args: light(0.0),
            check: None,
            secret: None,
        })
        .await
        .unwrap();
    // The store ticks once a second: the first queues the state (and fails), a later one
    // retries it while the state stays as it is.
    tokio::time::timeout(Duration::from_secs(5), async {
        while attempts.load(std::sync::atomic::Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the failed write is retried before any change or shutdown");
    assert_eq!(
        handle.snapshot.borrow().version,
        before + 1,
        "only the intent changed the state"
    );
    assert!(handle.shutdown().await);
    tokio::time::timeout(Duration::from_secs(10), actor)
        .await
        .unwrap()
        .unwrap();
}

/// A tile whose instance never serves its renderer is terminated and launched again,
/// instead of staying failed until someone relaunches it.
struct NotReadyOnceEc2 {
    launches: std::sync::atomic::AtomicU64,
    terminated: Arc<std::sync::Mutex<Vec<String>>>,
    calls: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl ec2::Ec2Driver for NotReadyOnceEc2 {
    async fn launch(&self, _tile: &str) -> Result<ec2::Launched, ec2::LaunchError> {
        let n = self
            .launches
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.calls.lock().unwrap().push("launch".into());
        Ok(ec2::Launched {
            instance: format!("i-test-{n}"),
            frame_url: None,
            ipv6: None,
            ipv4: None,
        })
    }
    async fn ready(&self, _tile: &str, l: &ec2::Launched) -> anyhow::Result<ec2::Launched> {
        if l.instance == "i-test-0" {
            anyhow::bail!("i-test-0 did not serve /health");
        }
        Ok(l.clone())
    }
    async fn terminate(&self, instance: &str) -> anyhow::Result<()> {
        self.terminated.lock().unwrap().push(instance.to_string());
        Ok(())
    }
    async fn post_scene(
        &self,
        _tile: &str,
        _l: &ec2::Launched,
        _body: &fleet_core::protocol::RendererScene,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn post_budget(
        &self,
        _tile: &str,
        _l: &ec2::Launched,
        _body: &fleet_core::protocol::BudgetBody,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn health(&self, _tile: &str, _l: &ec2::Launched) -> anyhow::Result<serde_json::Value> {
        anyhow::bail!("no renderer")
    }
    async fn sweep(&self) -> anyhow::Result<usize> {
        self.calls.lock().unwrap().push("sweep".into());
        Ok(0)
    }
}

#[tokio::test]
async fn an_ec2_tile_that_never_gets_ready_is_terminated_and_launched_again() {
    let config = Config {
        ec2_max: 1,
        max_tiles: 1,
        ..Config::default()
    };
    let terminated = Arc::new(std::sync::Mutex::new(Vec::new()));
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let driver: Arc<dyn ec2::Ec2Driver> = Arc::new(NotReadyOnceEc2 {
        launches: Default::default(),
        terminated: terminated.clone(),
        calls: calls.clone(),
    });
    let (handle, _actor) = start(
        State::new(config, now_ms()),
        None,
        Some(driver),
        None,
        None,
        None,
    );
    let mut agent = FakeAgent::connect(&handle, "gpu-a", 1);
    agent.assign().await;
    agent.report("slot-1", "running", 1);
    let mut snaps = handle.snapshot.clone();
    let s = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let v: Value = serde_json::from_str(&snaps.borrow_and_update().text).unwrap();
            let s = v["state"].clone();
            if s["tiles"][1]["phase"] == "running" {
                return s;
            }
            snaps.changed().await.unwrap();
        }
    })
    .await
    .expect("the tile runs after its first instance failed readiness");
    assert_eq!(s["tiles"][1]["vm"], "i-test-1");
    assert_eq!(*terminated.lock().unwrap(), vec!["i-test-0".to_string()]);
    // The EC2 side swept for instances an earlier coordinator left before it launched any.
    assert_eq!(
        calls.lock().unwrap()[..2],
        ["sweep".to_string(), "launch".to_string()]
    );
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
            ipv6: None,
            ipv4: None,
        })
    }
    async fn ready(&self, _tile: &str, l: &ec2::Launched) -> anyhow::Result<ec2::Launched> {
        Ok(l.clone())
    }
    async fn terminate(&self, _instance: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn post_scene(
        &self,
        _tile: &str,
        _l: &ec2::Launched,
        _body: &fleet_core::protocol::RendererScene,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn post_budget(
        &self,
        _tile: &str,
        _l: &ec2::Launched,
        _body: &fleet_core::protocol::BudgetBody,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn health(&self, _tile: &str, _l: &ec2::Launched) -> anyhow::Result<serde_json::Value> {
        anyhow::bail!("no renderer")
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
    let (handle, _actor) = start(
        State::new(config, now_ms()),
        None,
        Some(driver),
        None,
        None,
        None,
    );
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
    let (handle, _actor) = start(State::new(config, now_ms()), None, None, None, None, None);
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

/// The coordinator's host launcher over `fake` in `regions`, polling every 50 ms.
fn host_config(
    fake: &Arc<fcvm::fleet::coord::hosts::FakeHosts>,
    regions: &[&str],
    awake_capacity: u32,
    on_demand_max: u32,
) -> fcvm::fleet::coord::hosts::HostConfig {
    fcvm::fleet::coord::hosts::HostConfig {
        driver: fake.clone(),
        regions: regions.iter().map(|r| r.to_string()).collect(),
        awake_capacity,
        on_demand_max,
        poll: Duration::from_millis(50),
        liveness: Duration::from_millis(100),
        capacity_retry: Duration::from_millis(300),
    }
}

/// Serves the page for `handle` and connects one viewer to it.
async fn page_viewer(handle: &Handle) -> Viewer {
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
    Viewer::connect(&url).await
}

/// Waits until `cond` holds for `fake`, or fails with its calls.
async fn until_hosts(
    fake: &Arc<fcvm::fleet::coord::hosts::FakeHosts>,
    what: &str,
    cond: impl Fn(&fcvm::fleet::coord::hosts::FakeHosts) -> bool,
) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !cond(fake) {
        assert!(
            std::time::Instant::now() < deadline,
            "{what}: calls {:?}",
            fake.calls()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The launch calls `fake` received, in order.
fn launches(fake: &fcvm::fleet::coord::hosts::FakeHosts) -> Vec<String> {
    fake.calls()
        .into_iter()
        .filter(|c| c.starts_with("launch "))
        .collect()
}

/// The coordinator launches the GPU hosts itself, spot first: awake at start it launches
/// the awake capacity, with no active viewer it terminates every host, and a viewer's
/// visible page launches them again. While it wakes, the hosts show in the snapshot.
#[tokio::test]
async fn no_active_viewer_terminates_the_gpu_hosts_and_one_launches_them() {
    use fcvm::fleet::coord::hosts::FakeHosts;
    let config = Config {
        power_managed: true,
        idle_scale_down_secs: 1,
        ..Config::default()
    };
    let fake = Arc::new(FakeHosts::default());
    let (handle, _actor) = start(
        State::new(config, now_ms()),
        None,
        None,
        None,
        Some(host_config(&fake, &["us-east-1"], 2, 8)),
        None,
    );
    let mut v = page_viewer(&handle).await;
    v.until(|s| s["viewers"]["active"] == 1).await;
    until_hosts(&fake, "awake at start", |f| {
        f.present("us-east-1").len() == 2
    })
    .await;
    assert_eq!(launches(&fake), vec!["launch us-east-1 spot 2"]);
    v.send(json!({"type": "visibility", "visible": false}))
        .await;
    v.until(|s| s["fleet"]["power"]["state"] == "asleep").await;
    until_hosts(&fake, "asleep", |f| f.present("us-east-1").is_empty()).await;
    v.send(json!({"type": "visibility", "visible": true})).await;
    let s = v
        .until(|s| {
            s["fleet"]["power"]["state"] == "waking"
                && s["fleet"]["power"]["hosts"]
                    .as_array()
                    .is_some_and(|h| h.len() == 2)
        })
        .await;
    for h in s["fleet"]["power"]["hosts"].as_array().unwrap() {
        assert!(h["host"].as_str().unwrap().starts_with("i-"), "{s}");
        assert_eq!(h["phases"][0]["phase"], "instance_pending", "{s}");
    }
    assert_eq!(
        launches(&fake),
        vec!["launch us-east-1 spot 2", "launch us-east-1 spot 2"]
    );
}

/// A spot request EC2 refuses for capacity is followed by the on-demand request in the
/// same pass: no list or wait comes between them (the Auto Scaling path waited for a failed
/// activity, then for the next poll). The next wake asks spot first again.
#[tokio::test]
async fn a_refused_spot_launch_launches_on_demand_in_the_same_pass() {
    use fcvm::fleet::coord::hosts::{FakeHosts, Market};
    let config = Config {
        power_managed: true,
        idle_scale_down_secs: 1,
        ..Config::default()
    };
    let fake = Arc::new(FakeHosts::default());
    fake.capacity
        .lock()
        .unwrap()
        .insert(("us-east-1".into(), Market::Spot), 0);
    let (handle, _actor) = start(
        State::new(config, now_ms()),
        None,
        None,
        None,
        Some(host_config(&fake, &["us-east-1"], 1, 8)),
        None,
    );
    let mut v = page_viewer(&handle).await;
    v.until(|s| s["viewers"]["active"] == 1).await;
    until_hosts(&fake, "on-demand host", |f| {
        f.present("us-east-1").len() == 1
    })
    .await;
    let calls = fake.calls();
    let spot = calls
        .iter()
        .position(|c| c == "launch us-east-1 spot 1")
        .expect("spot asked first");
    assert_eq!(
        calls[spot + 1],
        "launch us-east-1 on-demand 1",
        "the on-demand launch did not follow the refusal at once: {calls:?}"
    );
    assert_eq!(fake.present("us-east-1")[0].market, Market::OnDemand);
    // Running: nothing more is launched.
    fake.set_state("us-east-1", "pending", "running");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(launches(&fake).len(), 2, "{:?}", fake.calls());
    // Sleep, then wake with spot capacity: spot first again, no on-demand.
    v.send(json!({"type": "visibility", "visible": false}))
        .await;
    v.until(|s| s["fleet"]["power"]["state"] == "asleep").await;
    until_hosts(&fake, "asleep", |f| f.present("us-east-1").is_empty()).await;
    fake.capacity.lock().unwrap().clear();
    v.send(json!({"type": "visibility", "visible": true})).await;
    until_hosts(&fake, "spot host", |f| f.present("us-east-1").len() == 1).await;
    assert_eq!(
        launches(&fake)[2..],
        ["launch us-east-1 spot 1".to_string()]
    );
}

/// The fleet never runs more on-demand hosts than the cap, all regions together: with
/// spot refused in two regions and a cap of 1, one region gets an on-demand host and the
/// other waits for capacity (asking spot again, never on demand).
#[tokio::test]
async fn on_demand_hosts_stay_within_the_cap_across_regions() {
    use fcvm::fleet::coord::hosts::{FakeHosts, Market};
    let fake = Arc::new(FakeHosts::default());
    for r in ["us-east-1", "us-east-2"] {
        fake.capacity
            .lock()
            .unwrap()
            .insert((r.into(), Market::Spot), 0);
    }
    let (_handle, _actor) = start(
        State::new(Config::default(), now_ms()),
        None,
        None,
        None,
        Some(host_config(&fake, &["us-east-1", "us-east-2"], 1, 1)),
        None,
    );
    until_hosts(&fake, "one on-demand host", |f| {
        f.present("us-east-1").len() + f.present("us-east-2").len() == 1
    })
    .await;
    // Past several capacity retries.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let od: Vec<String> = launches(&fake)
        .into_iter()
        .filter(|c| c.contains("on-demand"))
        .collect();
    assert_eq!(od.len(), 1, "{:?}", fake.calls());
    assert_eq!(
        fake.present("us-east-1").len() + fake.present("us-east-2").len(),
        1
    );
    assert!(
        launches(&fake)
            .iter()
            .filter(|c| c.starts_with("launch us-east-2 spot"))
            .count()
            >= 2,
        "the short region did not ask spot again: {:?}",
        fake.calls()
    );
}

/// A region that cannot be listed leaves the fleet's on-demand hosts uncounted, so no region
/// falls back to on demand until it can be: the cap fails closed.
#[tokio::test]
async fn an_uncounted_region_holds_back_the_on_demand_fallback() {
    use fcvm::fleet::coord::hosts::{FakeHosts, Market};
    let fake = Arc::new(FakeHosts::default());
    fake.capacity
        .lock()
        .unwrap()
        .insert(("us-east-1".into(), Market::Spot), 0);
    fake.fail_lists
        .lock()
        .unwrap()
        .insert("us-east-2".into(), 3);
    let (_handle, _actor) = start(
        State::new(Config::default(), now_ms()),
        None,
        None,
        None,
        Some(host_config(&fake, &["us-east-1", "us-east-2"], 1, 8)),
        None,
    );
    until_hosts(&fake, "us-east-2 listed", |f| {
        f.fail_lists.lock().unwrap()["us-east-2"] == 0 && !f.present("us-east-2").is_empty()
    })
    .await;
    let calls = fake.calls();
    let first_od = calls
        .iter()
        .position(|c| c == "launch us-east-1 on-demand 1")
        .expect("on demand once both regions are counted");
    let listed_ok = calls
        .iter()
        .enumerate()
        .filter(|(_, c)| *c == "list us-east-2")
        .map(|(i, _)| i)
        .nth(3)
        .expect("a fourth list of us-east-2");
    assert!(
        first_od > listed_ok,
        "on demand launched while us-east-2 was uncounted: {calls:?}"
    );
}

/// A host that is reclaimed (spot) or terminated while the fleet is awake is replaced.
#[tokio::test]
async fn a_reclaimed_host_is_replaced() {
    use fcvm::fleet::coord::hosts::FakeHosts;
    let fake = Arc::new(FakeHosts::default());
    *fake.launch_state.lock().unwrap() = Some("running".into());
    let (_handle, _actor) = start(
        State::new(Config::default(), now_ms()),
        None,
        None,
        None,
        Some(host_config(&fake, &["us-east-1"], 1, 8)),
        None,
    );
    until_hosts(&fake, "first host", |f| f.present("us-east-1").len() == 1).await;
    fake.set_state("us-east-1", "running", "terminated");
    until_hosts(&fake, "replacement", |f| f.present("us-east-1").len() == 1).await;
    assert_eq!(launches(&fake).len(), 2, "{:?}", fake.calls());
}

/// A launch whose answer is lost (EC2 launched, the reply never came) is asked again with
/// the same token, so EC2 answers with the instance it launched and no second host starts,
/// although no list names the first one yet.
#[tokio::test]
async fn a_launch_whose_answer_is_lost_is_asked_again_with_its_token() {
    use fcvm::fleet::coord::hosts::FakeHosts;
    let fake = Arc::new(FakeHosts::default());
    *fake.ambiguous.lock().unwrap() = 1;
    *fake.hide_new.lock().unwrap() = Some((0, 4));
    let (_handle, _actor) = start(
        State::new(Config::default(), now_ms()),
        None,
        None,
        None,
        Some(host_config(&fake, &["us-east-1"], 1, 8)),
        None,
    );
    until_hosts(&fake, "asked again", |f| launches(f).len() >= 2).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    // Every instance ever launched, terminated ones included: a second one would be
    // terminated as excess once both are listed, so the present count cannot tell.
    let launched = fake.hosts.lock().unwrap()["us-east-1"].len();
    assert_eq!(
        launched,
        1,
        "a second host for one shortfall: {:?}",
        fake.calls()
    );
}

/// An on-demand launch whose answer is lost counts against the fleet-wide cap until its
/// retry answers: with a cap of 1 and spot refused in two regions, the second region does
/// not launch on demand beside a host the first may have.
#[tokio::test]
async fn a_lost_on_demand_launch_holds_its_place_under_the_cap() {
    use fcvm::fleet::coord::hosts::{FakeHosts, Market};
    let fake = Arc::new(FakeHosts::default());
    for r in ["us-east-1", "us-east-2"] {
        fake.capacity
            .lock()
            .unwrap()
            .insert((r.into(), Market::Spot), 0);
    }
    *fake.ambiguous.lock().unwrap() = 1;
    *fake.hide_new.lock().unwrap() = Some((0, 4));
    let (_handle, _actor) = start(
        State::new(Config::default(), now_ms()),
        None,
        None,
        None,
        Some(host_config(&fake, &["us-east-1", "us-east-2"], 1, 1)),
        None,
    );
    until_hosts(&fake, "an on-demand launch", |f| {
        launches(f).iter().any(|c| c.contains("on-demand"))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let hosts = fake.hosts.lock().unwrap().clone();
    let od = hosts
        .values()
        .flatten()
        .filter(|h| h.market == Market::OnDemand)
        .count();
    assert_eq!(
        od,
        1,
        "on-demand instances over the cap: {:?}",
        fake.calls()
    );
}

/// A lost on-demand launch whose host the next list shows releases its reservation: the
/// region is full, its token is dropped, and the host is counted once. With a cap of 2, the
/// other region still gets an on-demand host when its spot host is reclaimed and spot is
/// refused.
#[tokio::test]
async fn a_lost_launch_whose_host_is_listed_releases_its_reservation() {
    use fcvm::fleet::coord::hosts::{FakeHosts, Market};
    let fake = Arc::new(FakeHosts::default());
    fake.capacity.lock().unwrap().extend([
        (("us-east-1".to_string(), Market::Spot), 0),
        (("us-east-2".to_string(), Market::Spot), 1),
    ]);
    *fake.ambiguous.lock().unwrap() = 1;
    let (_handle, _actor) = start(
        State::new(Config::default(), now_ms()),
        None,
        None,
        None,
        Some(host_config(&fake, &["us-east-1", "us-east-2"], 1, 2)),
        None,
    );
    until_hosts(&fake, "a host in each region", |f| {
        f.present("us-east-1").len() == 1 && f.present("us-east-2").len() == 1
    })
    .await;
    // Past the pass that lists the lost launch's host.
    tokio::time::sleep(Duration::from_millis(300)).await;
    fake.set_state("us-east-2", "pending", "terminated");
    until_hosts(&fake, "an on-demand host in us-east-2", |f| {
        f.present("us-east-2")
            .iter()
            .any(|h| h.market == Market::OnDemand)
    })
    .await;
}

/// A lost launch that launched part of its count is asked again with its token, and the
/// instances its answer names that the list already shows are not counted twice: only the
/// one host still missing falls back to on demand.
#[tokio::test]
async fn a_lost_partial_launch_counts_its_listed_hosts_once() {
    use fcvm::fleet::coord::hosts::{FakeHosts, Market};
    let fake = Arc::new(FakeHosts::default());
    fake.capacity
        .lock()
        .unwrap()
        .insert(("us-east-1".into(), Market::Spot), 2);
    *fake.ambiguous.lock().unwrap() = 1;
    let (_handle, _actor) = start(
        State::new(Config::default(), now_ms()),
        None,
        None,
        None,
        Some(host_config(&fake, &["us-east-1"], 3, 8)),
        None,
    );
    until_hosts(&fake, "three hosts", |f| f.present("us-east-1").len() == 3).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let hosts = fake.hosts.lock().unwrap()["us-east-1"].clone();
    let count = |m: Market| hosts.iter().filter(|h| h.market == m).count();
    assert_eq!(
        (count(Market::Spot), count(Market::OnDemand)),
        (2, 1),
        "{:?}",
        fake.calls()
    );
}

/// A list can name a new host and the next one leave it out (DescribeInstances is eventually
/// consistent): the host still counts, and no second one is launched.
#[tokio::test]
async fn a_host_a_later_list_leaves_out_is_not_launched_again() {
    use fcvm::fleet::coord::hosts::FakeHosts;
    let fake = Arc::new(FakeHosts::default());
    *fake.hide_new.lock().unwrap() = Some((1, 4));
    let (_handle, _actor) = start(
        State::new(Config::default(), now_ms()),
        None,
        None,
        None,
        Some(host_config(&fake, &["us-east-1"], 1, 8)),
        None,
    );
    until_hosts(&fake, "launched", |f| !launches(f).is_empty()).await;
    // Past the one list that names it, the four that leave it out, and more.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(launches(&fake).len(), 1, "{:?}", fake.calls());
    assert_eq!(fake.present("us-east-1").len(), 1);
}

/// A coordinator that restarts keeps the hosts it finds (an on-demand fallback from before
/// the restart included): nothing is launched beside them.
#[tokio::test]
async fn a_restart_keeps_the_hosts_it_finds() {
    use fcvm::fleet::coord::hosts::{FakeHosts, Host, Market};
    let fake = Arc::new(FakeHosts::default());
    fake.hosts.lock().unwrap().insert(
        "us-east-1".into(),
        vec![Host {
            id: "i-0123456789abcdef0".into(),
            state: "running".into(),
            market: Market::OnDemand,
        }],
    );
    let (_handle, _actor) = start(
        State::new(Config::default(), now_ms()),
        None,
        None,
        None,
        Some(host_config(&fake, &["us-east-1"], 1, 8)),
        None,
    );
    until_hosts(&fake, "listed", |f| {
        f.calls().iter().filter(|c| c.starts_with("list")).count() >= 3
    })
    .await;
    assert!(launches(&fake).is_empty(), "{:?}", fake.calls());
    assert_eq!(fake.present("us-east-1").len(), 1);
}

/// No poster is taken while tiles are starting or a race relaunches them: their regions
/// would be black. (On a GPU host the first posters of two runs were black frames, taken
/// mid-race and while the first VMs started.)
#[tokio::test]
async fn no_poster_is_planned_while_tiles_start_or_race() {
    let config = Config {
        restart_cooldown_secs: 0,
        ..Config::default()
    };
    let (handle, _actor) = start(State::new(config, now_ms()), None, None, None, None, None);
    let mut agent = FakeAgent::connect(&handle, "gpu-a", 1);
    agent.assign().await;
    agent
        .to_coord
        .send(AgentMsg::Aggregate {
            frame_url: "ws://gpu-host.example:20200/frames".into(),
        })
        .unwrap();
    // Nor while the tiles are still starting (the first poster of a GPU run was black).
    agent.report("slot-1", "restoring", 1);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        handle.poster_plan().await.is_none(),
        "a poster planned while a tile starts"
    );
    agent.report("slot-1", "running", 1);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while handle
        .poster_plan()
        .await
        .is_none_or(|p| p.sources.is_empty())
    {
        assert!(
            std::time::Instant::now() < deadline,
            "no plan before the race"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
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
    assert!(
        handle.poster_plan().await.is_none(),
        "a poster planned during a race"
    );
    agent.report("slot-1", "running", 2);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while handle.poster_plan().await.is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "no plan after the race"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A poster capture waits for a frame from every source, so an aggregator with no region
/// (a host whose GPUs are all on standby, or the EC2 side with no tile) must not be one:
/// it never streams, and the whole capture would be discarded.
#[tokio::test]
async fn a_poster_plan_names_only_placed_aggregators() {
    let config = Config {
        max_tiles: 1,
        ..Config::default()
    };
    let (handle, _actor) = start(
        State::new(config, now_ms()),
        None,
        None,
        Some("ws://127.0.0.1:9/frames".into()),
        None,
        None,
    );
    let mut a = FakeAgent::connect(&handle, "gpu-a", 1);
    let slots = a.assign().await;
    assert!(slots[0].rect.is_some());
    a.report("slot-1", "running", 1);
    a.to_coord
        .send(AgentMsg::Aggregate {
            frame_url: "ws://gpu-a.example:20200/frames".into(),
        })
        .unwrap();
    // gpu-b's only GPU is on standby: max_tiles is taken by gpu-a.
    let b = FakeAgent::connect(&handle, "gpu-b", 1);
    b.to_coord
        .send(AgentMsg::Aggregate {
            frame_url: "ws://gpu-b.example:20200/frames".into(),
        })
        .unwrap();
    // A scrub after it on the same connection shows when gpu-b's aggregate is applied.
    b.to_coord
        .send(AgentMsg::Scrub {
            slot: "slot-2".into(),
            sbdf: "0000:01:00.0".into(),
            ok: true,
            error: None,
        })
        .unwrap();
    let mut snaps = handle.snapshot.clone();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let v: Value = serde_json::from_str(&snaps.borrow_and_update().text).unwrap();
            if v["state"]["counters"]["scrubs"] == 1 {
                return;
            }
            snaps.changed().await.unwrap();
        }
    })
    .await
    .expect("gpu-b's messages applied");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let plan = loop {
        if let Some(p) = handle
            .poster_plan()
            .await
            .filter(|p| p.sources.iter().any(|u| u.contains("gpu-a")))
        {
            break p;
        }
        assert!(std::time::Instant::now() < deadline, "no plan");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(
        plan.sources,
        vec!["ws://gpu-a.example:20200/frames".to_string()]
    );
}

/// An agent that dials in over `/agent` speaks the same messages, one per text frame: its
/// hello brings the scene and the assignment, and closing the socket takes the host out.
#[tokio::test]
async fn an_agent_can_dial_the_agent_websocket() {
    let c = coordinator_with(Opts::default()).await;
    let agent_url = c.url.replace("/live", "/agent");
    let (mut ws, _) = tokio_tungstenite::connect_async(agent_url.as_str())
        .await
        .unwrap();
    let hello = json!({"type": "hello", "host": "gpu-dial", "arch": "x86_64",
        "gpus": [{"sbdf": "0000:01:00.0", "name": "Tesla T4"}], "capacity": 1});
    ws.send(Message::Text(hello.to_string().into()))
        .await
        .unwrap();
    let mut kinds = Vec::new();
    while kinds.len() < 2 {
        let m = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if let Message::Text(t) = m {
            let v: Value = serde_json::from_str(&t).unwrap();
            kinds.push(v["type"].as_str().unwrap().to_string());
        }
    }
    assert_eq!(kinds, vec!["scene", "assign"]);
    let mut v = Viewer::connect(&c.url).await;
    v.until(|s| s["fleet"]["n"] == 1).await;
    ws.close(None).await.unwrap();
    v.until(|s| s["fleet"]["n"] == 0).await;
}

/// A fake S3: PUT stores the body at the path, GET returns it or 404, and every request
/// must be SigV4-signed with the payload's hash. `deny` answers 403 to everything.
#[derive(Clone, Default)]
struct FakeS3 {
    objects: Arc<std::sync::Mutex<BTreeMap<String, Vec<u8>>>>,
    deny: Arc<std::sync::atomic::AtomicBool>,
}

async fn fake_s3(s3: FakeS3) -> String {
    use axum::body::Bytes;
    use axum::http::{HeaderMap, Method, StatusCode, Uri};
    let app = axum::Router::new()
        .fallback(
            |axum::extract::State(s3): axum::extract::State<FakeS3>,
             method: Method,
             uri: Uri,
             headers: HeaderMap,
             body: Bytes| async move {
                use sha2::Digest;
                if s3.deny.load(std::sync::atomic::Ordering::SeqCst) {
                    return (StatusCode::FORBIDDEN, Vec::new());
                }
                let auth = headers
                    .get("authorization")
                    .map(|v| v.to_str().unwrap().to_string());
                assert!(
                    auth.as_deref().is_some_and(|a| a
                        .starts_with("AWS4-HMAC-SHA256 Credential=AKIDFORTESTS/")
                        && a.contains("/s3/aws4_request")),
                    "{auth:?}"
                );
                assert_eq!(
                    headers["x-amz-content-sha256"].to_str().unwrap(),
                    hex::encode(sha2::Sha256::digest(&body))
                );
                let mut o = s3.objects.lock().unwrap();
                match method {
                    Method::PUT => {
                        o.insert(uri.path().to_string(), body.to_vec());
                        (StatusCode::OK, Vec::new())
                    }
                    Method::GET => match o.get(uri.path()) {
                        Some(b) => (StatusCode::OK, b.clone()),
                        None => (StatusCode::NOT_FOUND, Vec::new()),
                    },
                    _ => (StatusCode::METHOD_NOT_ALLOWED, Vec::new()),
                }
            },
        )
        .with_state(s3);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    base
}

/// With --state-bucket the state is one S3 object: none at first, the last state written
/// at shutdown, read back whole; a refused read is an error, never an empty state.
#[tokio::test]
async fn the_state_lives_in_s3_and_a_refused_read_is_an_error() {
    std::env::set_var("AWS_ACCESS_KEY_ID", "AKIDFORTESTS");
    std::env::set_var("AWS_SECRET_ACCESS_KEY", "secret-for-tests");
    let s3 = FakeS3::default();
    let base = fake_s3(s3.clone()).await;
    let obj =
        || store::S3Object::with_endpoint("fleet-state", "state.json", "us-east-2", &base).unwrap();
    assert!(obj().load().await.unwrap().is_none(), "no object yet");
    let (handle, actor) = start(
        State::new(
            Config {
                light_interval_ms: 0,
                ..Config::default()
            },
            1_000,
        ),
        Some(store::spawn_s3_writer(obj())),
        None,
        None,
        None,
        None,
    );
    let mut last = 0;
    for i in 0..3 {
        last = handle
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
            .unwrap()
            .version;
    }
    // The coordinator's 1 s ticker moves the version without a receipt whenever a tick
    // changes the fleet, so the stored version can pass the last receipt's. A viewer
    // joining does the same on demand, which makes that happen on every run.
    let after = handle
        .apply(Event::ViewerJoined {
            at: 0,
            viewer: "v-2".into(),
            client: None,
        })
        .await;
    assert!(after.is_none(), "a join carries no receipt");
    handle.shutdown().await;
    tokio::time::timeout(Duration::from_secs(5), actor)
        .await
        .unwrap()
        .unwrap();
    assert!(s3
        .objects
        .lock()
        .unwrap()
        .contains_key("/fleet-state/state.json"));
    let back = obj().load().await.unwrap().expect("the state object");
    // The stored state is the state the actor ended with, which the shutdown publishes.
    let final_version = handle.snapshot.borrow().version;
    assert_eq!(back.version, final_version);
    assert!(
        back.version > last,
        "the join moved the version past the last receipt"
    );
    for i in 0..3 {
        assert!(
            back.receipts.contains_key(&format!("l-{i}")),
            "receipt l-{i}"
        );
    }
    s3.deny.store(true, std::sync::atomic::Ordering::SeqCst);
    let e = obj().load().await.unwrap_err();
    assert!(format!("{e:#}").contains("403"), "{e:#}");
}

/// One EC2 API call: its region and its parameters.
type Ec2Call = (String, BTreeMap<String, String>);

/// A fake EC2 query API. `spot_capacity` lists the regions with spot capacity; on-demand
/// launches always succeed. Calls are recorded as (region, params).
#[derive(Clone, Default)]
struct FakeEc2Api {
    spot_capacity: Vec<String>,
    /// Per region, the instances a tag-filtered DescribeInstances finds.
    tagged: BTreeMap<String, Vec<String>>,
    /// Instances DescribeInstances reports terminated.
    gone: Vec<String>,
    calls: Arc<std::sync::Mutex<Vec<Ec2Call>>>,
}

async fn fake_ec2_api(api: FakeEc2Api) -> String {
    use axum::http::{HeaderMap, StatusCode};
    let app = axum::Router::new()
        .fallback(
            |axum::extract::State(api): axum::extract::State<FakeEc2Api>,
             headers: HeaderMap,
             body: String| async move {
                let auth = headers["authorization"].to_str().unwrap().to_string();
                let region = auth.split('/').nth(2).unwrap().to_string();
                let params: BTreeMap<String, String> = body
                    .split('&')
                    .filter_map(|kv| kv.split_once('='))
                    .map(|(k, v)| {
                        let dec = |s: &str| {
                            let mut out = Vec::new();
                            let b = s.as_bytes();
                            let mut i = 0;
                            while i < b.len() {
                                if b[i] == b'%' {
                                    out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
                                    i += 3;
                                } else {
                                    out.push(b[i]);
                                    i += 1;
                                }
                            }
                            String::from_utf8(out).unwrap()
                        };
                        (dec(k), dec(v))
                    })
                    .collect();
                api.calls.lock().unwrap().push((region.clone(), params.clone()));
                let action = params["Action"].as_str();
                let error = |code: &str| {
                    (
                        StatusCode::BAD_REQUEST,
                        format!("<Response><Errors><Error><Code>{code}</Code><Message>none here</Message></Error></Errors></Response>"),
                    )
                };
                match action {
                    "RunInstances" => {
                        let spot = params["LaunchTemplate.LaunchTemplateName"] == "fcvm-fleet-tile-x86_64";
                        if spot && !api.spot_capacity.contains(&region) {
                            return error("InsufficientInstanceCapacity");
                        }
                        (StatusCode::OK, "<RunInstancesResponse><instancesSet><item><instanceId>i-0123456789abcdef0</instanceId></item></instancesSet></RunInstancesResponse>".into())
                    }
                    "DescribeInstances" if params.contains_key("Filter.1.Name") => {
                        let items: String = api
                            .tagged
                            .get(&region)
                            .into_iter()
                            .flatten()
                            .map(|id| format!("<item><instancesSet><item><instanceId>{id}</instanceId><instanceState><code>16</code><name>running</name></instanceState></item></instancesSet></item>"))
                            .collect();
                        (StatusCode::OK, format!("<DescribeInstancesResponse><reservationSet>{items}</reservationSet></DescribeInstancesResponse>"))
                    }
                    "DescribeInstances" if api.gone.contains(&params["InstanceId.1"]) => (StatusCode::OK, format!("<DescribeInstancesResponse><reservationSet><item><instancesSet><item><instanceId>{}</instanceId><instanceState><code>48</code><name>terminated</name></instanceState></item></instancesSet></item></reservationSet></DescribeInstancesResponse>", params["InstanceId.1"])),
                    "DescribeInstances" => (StatusCode::OK, "<DescribeInstancesResponse><reservationSet><item><instancesSet><item><instanceId>i-0123456789abcdef0</instanceId><instanceState><code>16</code><name>running</name></instanceState><ipAddress>127.0.0.1</ipAddress><ipv6Address>2001:db8::5</ipv6Address></item></instancesSet></item></reservationSet></DescribeInstancesResponse>".into()),
                    "TerminateInstances" => (StatusCode::OK, "<TerminateInstancesResponse/>".into()),
                    // An instant EC2 Fleet: spot is refused per pool where the region has no spot
                    // capacity (a 200 with an errorSet, as EC2 answers it); on demand launches.
                    "CreateFleet" => {
                        let spot = params["TargetCapacitySpecification.DefaultTargetCapacityType"] == "spot";
                        if spot && !api.spot_capacity.contains(&region) {
                            return (StatusCode::OK, "<CreateFleetResponse><fleetId>fleet-0</fleetId><errorSet><item><lifecycle>spot</lifecycle><errorCode>UnfulfillableCapacity</errorCode><errorMessage>Unable to fulfill capacity due to your request configuration.</errorMessage></item></errorSet><fleetInstanceSet/></CreateFleetResponse>".into());
                        }
                        (StatusCode::OK, "<CreateFleetResponse><fleetId>fleet-1</fleetId><fleetInstanceSet><item><instanceIds><item>i-0fedcba9876543210</item></instanceIds></item></fleetInstanceSet></CreateFleetResponse>".into())
                    }
                    _ => error("InvalidAction"),
                }
            },
        )
        .with_state(api);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    base
}

/// The host driver asks EC2 Fleet for every template and subnet of the region at once, reads a
/// spot refusal for capacity out of the answer's errorSet (EC2 answers 200), and an on-demand
/// request launches; the fleet's hosts are listed by their tags.
#[tokio::test]
async fn the_aws_host_driver_launches_through_an_instant_fleet() {
    use fcvm::fleet::coord::hosts::{AwsHosts, HostDriver, Market};
    std::env::set_var("AWS_ACCESS_KEY_ID", "AKIDFORTESTS");
    std::env::set_var("AWS_SECRET_ACCESS_KEY", "secret-for-tests");
    let api = FakeEc2Api {
        tagged: [(
            "us-east-1".to_string(),
            vec!["i-0123456789abcdef0".to_string()],
        )]
        .into(),
        ..Default::default()
    };
    let base = fake_ec2_api(api.clone()).await;
    let mut d = AwsHosts::new(
        &["us-east-1:fcvm-fleet-host-x86_64".parse().unwrap()],
        &[
            "us-east-1:subnet-0000000000000000a".parse().unwrap(),
            "us-east-1:subnet-0000000000000000b".parse().unwrap(),
        ],
        "stage".into(),
    )
    .unwrap();
    d.endpoints.insert("us-east-1".into(), base);
    let spot = d
        .launch("us-east-1", Market::Spot, 1, "token-spot")
        .await
        .unwrap();
    assert!(spot.ids.is_empty(), "{spot:?}");
    assert_eq!(spot.refused.len(), 1, "{spot:?}");
    assert!(spot.refused[0].starts_with("UnfulfillableCapacity: "));
    let od = d
        .launch("us-east-1", Market::OnDemand, 1, "token-od")
        .await
        .unwrap();
    assert_eq!(od.ids, vec!["i-0fedcba9876543210".to_string()]);
    let calls = api.calls.lock().unwrap().clone();
    let (region, p) = &calls[0];
    assert_eq!(region, "us-east-1");
    assert_eq!(p["Action"], "CreateFleet");
    assert_eq!(p["ClientToken"], "token-spot");
    assert_eq!(p["Type"], "instant");
    assert_eq!(
        p["LaunchTemplateConfigs.1.LaunchTemplateSpecification.LaunchTemplateName"],
        "fcvm-fleet-host-x86_64"
    );
    assert_eq!(
        p["LaunchTemplateConfigs.1.Overrides.2.SubnetId"],
        "subnet-0000000000000000b"
    );
    assert_eq!(p["TagSpecification.1.Tag.2.Value"], "fcvm-fleet-host");
    let hosts = d.hosts("us-east-1").await.unwrap();
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0].id, "i-0123456789abcdef0");
    let list = &api.calls.lock().unwrap()[2].1;
    assert_eq!(list["Filter.1.Value.1"], "fcvm-fleet-host");
    assert_eq!(list["Filter.2.Value.1"], "stage");
    // A template with no subnet in its region is refused at start.
    assert!(AwsHosts::new(&["us-east-1:t".parse().unwrap()], &[], "stage".into()).is_err());
}

/// `alive` reads the instance's state and asks its renderer: a running tile whose renderer
/// answers is alive, a terminated one (a spot reclaim) or one whose renderer reports an
/// error is dead, and one whose renderer does not answer is silent.
#[tokio::test]
async fn the_aws_tile_driver_sees_a_reclaimed_instance_and_a_stopped_renderer() {
    use fcvm::fleet::coord::ec2::{Ec2Driver, Liveness};
    use fcvm::fleet::coord::ec2_aws::AwsEc2;
    std::env::set_var("AWS_ACCESS_KEY_ID", "AKIDFORTESTS");
    std::env::set_var("AWS_SECRET_ACCESS_KEY", "secret-for-tests");
    let api = FakeEc2Api {
        gone: vec!["i-reclaimed-1".to_string()],
        ..Default::default()
    };
    let base = fake_ec2_api(api.clone()).await;
    // A renderer answering `body` at /health, on a port of its own.
    async fn renderer(body: Value) -> u16 {
        let app = axum::Router::new().route(
            "/health",
            axum::routing::get(move || {
                let body = body.clone();
                async move { axum::Json(body) }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        port
    }
    let driver = |port: u16| {
        let mut d = AwsEc2::new(
            vec!["us-east-1".to_string()],
            "fcvm-fleet-tile-x86_64".into(),
            None,
            "fcvm.example".into(),
        )
        .unwrap();
        d.endpoints.insert("us-east-1".into(), base.clone());
        d.health_scheme = "http".into();
        d.health_port = port;
        d
    };
    let launched = |id: &str| ec2::Launched {
        instance: format!("us-east-1/{id}"),
        frame_url: None,
        ipv6: None,
        ipv4: None,
    };
    let running = launched("i-0123456789abcdef0");
    let ready = driver(renderer(json!({"ready": true})).await);
    assert_eq!(
        ready.alive("ec2-1", &running).await.unwrap(),
        Liveness::Alive
    );
    match ready
        .alive("ec2-1", &launched("i-reclaimed-1"))
        .await
        .unwrap()
    {
        Liveness::Dead(why) => assert!(why.contains("terminated"), "{why}"),
        other => panic!("a reclaimed instance is {other:?}"),
    }
    let failed = driver(renderer(json!({"ready": true, "error": "CUDA error 999"})).await);
    match failed.alive("ec2-1", &running).await.unwrap() {
        Liveness::Dead(why) => assert!(why.contains("CUDA error 999"), "{why}"),
        other => panic!("a failed renderer is {other:?}"),
    }
    // Nothing listens on a port that was bound and released.
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    match driver(closed).alive("ec2-1", &running).await.unwrap() {
        Liveness::Silent(_) => {}
        other => panic!("a renderer that does not answer is {other:?}"),
    }
}

/// A restarted coordinator does not know the tile instances an earlier one launched:
/// `sweep` finds every instance tagged `Purpose=fcvm-fleet-tile` that is not terminated, in
/// every fleet region, and terminates them.
#[tokio::test]
async fn the_aws_tile_driver_sweeps_every_tagged_tile_instance() {
    use fcvm::fleet::coord::ec2::Ec2Driver;
    use fcvm::fleet::coord::ec2_aws::AwsEc2;
    std::env::set_var("AWS_ACCESS_KEY_ID", "AKIDFORTESTS");
    std::env::set_var("AWS_SECRET_ACCESS_KEY", "secret-for-tests");
    let api = FakeEc2Api {
        tagged: BTreeMap::from([(
            "us-east-1".to_string(),
            vec!["i-swept-1".to_string(), "i-swept-2".to_string()],
        )]),
        ..Default::default()
    };
    let base = fake_ec2_api(api.clone()).await;
    let regions = vec!["us-east-1".to_string(), "us-west-2".to_string()];
    let mut d = AwsEc2::new(
        regions.clone(),
        "fcvm-fleet-tile-x86_64".into(),
        None,
        "fcvm.example".into(),
    )
    .unwrap();
    for r in &regions {
        d.endpoints.insert(r.clone(), base.clone());
    }
    d.fleet = "stage-test".into();
    assert_eq!(d.sweep().await.unwrap(), 2);
    let calls = api.calls.lock().unwrap();
    let describes: Vec<&Ec2Call> = calls
        .iter()
        .filter(|(_, p)| p["Action"] == "DescribeInstances")
        .collect();
    assert_eq!(describes.len(), 2, "one tag-filtered describe per region");
    for (_, p) in &describes {
        assert_eq!(p["Filter.1.Name"], "tag:Purpose");
        assert_eq!(p["Filter.1.Value.1"], "fcvm-fleet-tile");
        assert_eq!(p["Filter.2.Name"], "instance-state-name");
        // Only this fleet's tiles: another coordinator's in the account are left alone.
        assert_eq!(p["Filter.3.Name"], "tag:Fleet");
        assert_eq!(p["Filter.3.Value.1"], "stage-test");
    }
    let terminates: Vec<&Ec2Call> = calls
        .iter()
        .filter(|(_, p)| p["Action"] == "TerminateInstances")
        .collect();
    assert_eq!(terminates.len(), 1, "nothing to terminate in us-west-2");
    assert_eq!(terminates[0].0, "us-east-1");
    assert_eq!(terminates[0].1["InstanceId.1"], "i-swept-1");
    assert_eq!(terminates[0].1["InstanceId.2"], "i-swept-2");
}

/// The renderer answers `/health` with 200 while CUDA starts and after it fails, so a tile
/// is ready only when the body says `ready`, and a body with an `error` fails the launch
/// (the tile is then terminated and launched again) instead of being accepted.
#[tokio::test]
async fn the_aws_tile_driver_waits_for_a_ready_renderer_and_refuses_a_failed_one() {
    use fcvm::fleet::coord::ec2::Ec2Driver;
    use fcvm::fleet::coord::ec2_aws::AwsEc2;
    std::env::set_var("AWS_ACCESS_KEY_ID", "AKIDFORTESTS");
    std::env::set_var("AWS_SECRET_ACCESS_KEY", "secret-for-tests");
    // Serves `bodies` in turn, the last one from then on; counts the requests.
    async fn renderer(bodies: Vec<Value>) -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
        let n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let n2 = n.clone();
        let app = axum::Router::new().route(
            "/health",
            axum::routing::get(move || {
                let n2 = n2.clone();
                let bodies = bodies.clone();
                async move {
                    let i = n2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    axum::Json(bodies[i.min(bodies.len() - 1)].clone())
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (port, n)
    }
    let base = fake_ec2_api(FakeEc2Api::default()).await;
    let driver = |port: u16| {
        let mut d = AwsEc2::new(
            vec!["us-east-1".to_string()],
            "fcvm-fleet-tile-x86_64".into(),
            None,
            "fcvm.example".into(),
        )
        .unwrap();
        d.endpoints.insert("us-east-1".into(), base.clone());
        d.health_scheme = "http".into();
        d.health_port = port;
        d.poll = Duration::from_millis(20);
        d.ready_timeout = Duration::from_secs(5);
        d
    };
    let launched = ec2::Launched {
        instance: "us-east-1/i-0123456789abcdef0".into(),
        frame_url: None,
        ipv6: None,
        ipv4: None,
    };
    let (port, n) = renderer(vec![json!({"ready": false}), json!({"ready": true})]).await;
    driver(port).ready("ec2-1", &launched).await.unwrap();
    assert_eq!(
        n.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "a renderer that is not ready yet is asked again"
    );
    let (port, _) = renderer(vec![json!({"ready": false, "error": "CUDA init failed"})]).await;
    let e = driver(port)
        .ready("ec2-1", &launched)
        .await
        .expect_err("a failed renderer is not a ready tile");
    assert!(format!("{e:#}").contains("CUDA init failed"), "{e:#}");
}

/// The AWS tile driver: a spot launch from the template, tagged, in the first region with
/// capacity; ready once the instance runs and its renderer answers at `<tile>.<zone>`
/// (resolved to the instance itself); terminate by id in its region. With no spot capacity
/// anywhere it uses the on-demand template, and without one it reports no capacity.
#[tokio::test]
async fn the_aws_tile_driver_launches_waits_and_terminates() {
    use fcvm::fleet::coord::ec2::{Ec2Driver, LaunchError};
    use fcvm::fleet::coord::ec2_aws::AwsEc2;
    std::env::set_var("AWS_ACCESS_KEY_ID", "AKIDFORTESTS");
    std::env::set_var("AWS_SECRET_ACCESS_KEY", "secret-for-tests");
    // The tile's renderer, which must be asked for by the tile's name.
    let hosts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let h2 = hosts.clone();
    let scenes = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let s2 = scenes.clone();
    let renderer = axum::Router::new()
        .route(
            "/health",
            axum::routing::get(move |headers: axum::http::HeaderMap| {
                let h2 = h2.clone();
                async move {
                    h2.lock()
                        .unwrap()
                        .push(headers["host"].to_str().unwrap().to_string());
                    axum::Json(json!({"ready": true}))
                }
            }),
        )
        .route(
            "/scene",
            axum::routing::post(move |axum::Json(b): axum::Json<Value>| {
                let s2 = s2.clone();
                async move {
                    s2.lock().unwrap().push(b);
                    "ok"
                }
            }),
        );
    let rl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rport = rl.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(rl, renderer).await.unwrap() });
    let regions = vec!["us-east-1".to_string(), "us-west-2".to_string()];
    let driver = |api: &FakeEc2Api, base: &str, od: Option<&str>| {
        let mut d = AwsEc2::new(
            regions.clone(),
            "fcvm-fleet-tile-x86_64".into(),
            od.map(str::to_string),
            "fcvm.example".into(),
        )
        .unwrap();
        for r in &regions {
            d.endpoints.insert(r.clone(), base.to_string());
        }
        d.health_scheme = "http".into();
        d.health_port = rport;
        d.poll = Duration::from_millis(20);
        d.ready_timeout = Duration::from_secs(5);
        let _ = api;
        d
    };

    let api = FakeEc2Api {
        spot_capacity: vec!["us-west-2".into()],
        ..Default::default()
    };
    let base = fake_ec2_api(api.clone()).await;
    let d = driver(&api, &base, None);
    let l = d.launch("ec2-1").await.unwrap();
    assert_eq!(l.instance, "us-west-2/i-0123456789abcdef0");
    assert_eq!(
        l.frame_url.as_deref(),
        Some("wss://ec2-1.fcvm.example/frames")
    );
    {
        let calls = api.calls.lock().unwrap();
        assert_eq!(calls[0].0, "us-east-1", "the first region is tried first");
        let (region, p) = &calls[1];
        assert_eq!(region, "us-west-2");
        assert_eq!(p["LaunchTemplate.Version"], "$Latest");
        assert_eq!(p["TagSpecification.1.Tag.1.Key"], "Purpose");
        assert_eq!(p["TagSpecification.1.Tag.1.Value"], "fcvm-fleet-tile");
        assert_eq!(p["TagSpecification.1.Tag.2.Value"], "ec2-1");
        assert_eq!(p["TagSpecification.1.Tag.3.Key"], "Fleet");
        assert_eq!(p["TagSpecification.1.Tag.3.Value"], "stage");
    }
    let up = d.ready("ec2-1", &l).await.unwrap();
    assert_eq!(up.ipv4.as_deref(), Some("127.0.0.1"));
    assert_eq!(up.ipv6.as_deref(), Some("2001:db8::5"));
    assert_eq!(
        hosts.lock().unwrap()[0],
        format!("ec2-1.fcvm.example:{rport}")
    );
    // The tile's renderer gets its scene body, layout generation included.
    let mut body =
        fleet_core::protocol::SharedScene::new(&fleet_core::scene::Scene::new(1_000), 16.0 / 9.0)
            .for_renderer(fleet_core::tiling::Rect {
                x: 0.5,
                y: 0.0,
                w: 0.5,
                h: 1.0,
            });
    body.layout_generation = 7;
    body.rect_from_frame = 900;
    d.post_scene("ec2-1", &up, &body).await.unwrap();
    let got = scenes.lock().unwrap()[0].clone();
    assert_eq!(got["layout_generation"], json!(7));
    assert_eq!(got["rect_from_frame"], json!(900));
    assert_eq!(got["rect"]["x"], 0.5);
    d.terminate(&l.instance).await.unwrap();
    {
        let calls = api.calls.lock().unwrap();
        let (region, p) = calls.last().unwrap();
        assert_eq!(
            (region.as_str(), p["Action"].as_str()),
            ("us-west-2", "TerminateInstances")
        );
        assert_eq!(p["InstanceId.1"], "i-0123456789abcdef0");
    }

    // No spot capacity anywhere: the on-demand template, else no capacity.
    let api = FakeEc2Api::default();
    let base = fake_ec2_api(api.clone()).await;
    let l = driver(&api, &base, Some("fcvm-fleet-tile-od-x86_64"))
        .launch("ec2-2")
        .await
        .unwrap();
    assert_eq!(l.instance, "us-east-1/i-0123456789abcdef0");
    assert_eq!(
        api.calls.lock().unwrap().last().unwrap().1["LaunchTemplate.LaunchTemplateName"],
        "fcvm-fleet-tile-od-x86_64"
    );
    match driver(&api, &base, None).launch("ec2-3").await {
        Err(LaunchError::NoCapacity(why)) => {
            assert!(
                why.contains("us-east-1") && why.contains("us-west-2"),
                "{why}"
            )
        }
        other => panic!("expected no capacity, got {other:?}"),
    }
}

/// A fake Cloudflare DNS API for one zone: records by id, and every write recorded.
#[derive(Clone, Default)]
struct FakeCf {
    records: Arc<std::sync::Mutex<BTreeMap<String, Value>>>,
    writes: Arc<std::sync::Mutex<Vec<String>>>,
    next: Arc<std::sync::atomic::AtomicU64>,
}

async fn fake_cloudflare(cf: FakeCf) -> String {
    use axum::extract::{Path, State as St};
    use axum::routing::{get, put};
    let app = axum::Router::new()
        .route(
            "/zones/{zid}/dns_records",
            get(
                |St(cf): St<FakeCf>, headers: axum::http::HeaderMap| async move {
                    assert_eq!(headers["authorization"], "Bearer dns-token-for-tests");
                    let all: Vec<Value> = cf.records.lock().unwrap().values().cloned().collect();
                    axum::Json(
                        json!({"success": true, "result": all, "result_info": {"total_pages": 1}}),
                    )
                },
            )
            .post(
                |St(cf): St<FakeCf>, axum::Json(mut r): axum::Json<Value>| async move {
                    let id = format!(
                        "r{}",
                        cf.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    );
                    r["id"] = json!(id);
                    cf.writes.lock().unwrap().push(format!(
                        "create {} {} {}",
                        r["type"], r["name"], r["content"]
                    ));
                    cf.records.lock().unwrap().insert(id.clone(), r.clone());
                    axum::Json(json!({"success": true, "result": r}))
                },
            ),
        )
        .route(
            "/zones/{zid}/dns_records/{id}",
            put(
                |St(cf): St<FakeCf>,
                 Path((_z, id)): Path<(String, String)>,
                 axum::Json(mut r): axum::Json<Value>| async move {
                    r["id"] = json!(id);
                    cf.writes
                        .lock()
                        .unwrap()
                        .push(format!("update {} {}", r["name"], r["content"]));
                    cf.records.lock().unwrap().insert(id, r.clone());
                    axum::Json(json!({"success": true, "result": r}))
                },
            )
            .delete(
                |St(cf): St<FakeCf>, Path((_z, id)): Path<(String, String)>| async move {
                    let r = cf.records.lock().unwrap().remove(&id).unwrap();
                    cf.writes
                        .lock()
                        .unwrap()
                        .push(format!("delete {}", r["name"]));
                    axum::Json(json!({"success": true, "result": {"id": id}}))
                },
            ),
        )
        .with_state(cf);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    base
}

/// A confirmation the test opens: until it does, every launch's names count as not yet
/// answering at the authoritative servers. It also checks, when asked, that the records it
/// is asked about are already in the zone: confirmation comes after the write.
#[derive(Clone)]
struct GateConfirm {
    open: Arc<std::sync::atomic::AtomicBool>,
    cf: FakeCf,
    asked_before_written: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl fcvm::fleet::coord::dns::Confirm for GateConfirm {
    async fn confirmed(&self, records: &[fcvm::fleet::coord::dns::Record]) -> bool {
        let zone = self.cf.records.lock().unwrap().clone();
        for r in records {
            if !zone
                .values()
                .any(|z| z["name"] == r.name.as_str() && z["content"] == r.content.as_str())
            {
                self.asked_before_written
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        self.open.load(std::sync::atomic::Ordering::SeqCst)
    }
}

struct DnsFixture {
    cf: FakeCf,
    gate: GateConfirm,
    handle: Handle,
    agent: FakeAgent,
}

async fn dns_fixture(pre: &[(&str, &str, &str)]) -> DnsFixture {
    use fcvm::fleet::coord::dns::Cloudflare;
    let cf = FakeCf::default();
    for (i, (kind, name, content)) in pre.iter().enumerate() {
        cf.records.lock().unwrap().insert(
            format!("pre{i}"),
            json!({"id": format!("pre{i}"), "type": kind, "name": name, "content": content, "proxied": false}),
        );
    }
    let base = fake_cloudflare(cf.clone()).await;
    let gate = GateConfirm {
        open: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        cf: cf.clone(),
        asked_before_written: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let writer = Cloudflare::with_endpoint(
        "dns-token-for-tests".into(),
        "fcvm.example".into(),
        Some("zone-for-tests".into()),
        &base,
    )
    .unwrap()
    .with_confirm(Arc::new(gate.clone()));
    let (handle, _actor) = start(
        State::new(Config::default(), now_ms()),
        None,
        None,
        None,
        None,
        Some(writer),
    );
    let mut agent = FakeAgent::connect(&handle, "gpu-a", 1);
    agent.assign().await;
    DnsFixture {
        cf,
        gate,
        handle,
        agent,
    }
}

impl DnsFixture {
    fn report(&self, phase: &str, generation: u64, v6: Option<&str>, v4: Option<&str>) {
        self.agent
            .to_coord
            .send(AgentMsg::Slot(SlotReport {
                id: "slot-1".into(),
                phase: phase.into(),
                generation,
                ipv6: v6.map(str::to_string),
                ipv4: v4.map(str::to_string),
                v4_port: v4.map(|_| 20200),
                ..Default::default()
            }))
            .unwrap();
    }

    fn tile(&self) -> Value {
        serde_json::from_str::<Value>(&self.handle.snapshot.borrow().text).unwrap()["state"]
            ["tiles"][0]
            .clone()
    }

    /// The zone's records as (type, name, content), sorted.
    fn zone(&self) -> Vec<(String, String, String)> {
        let mut v: Vec<_> = self
            .cf
            .records
            .lock()
            .unwrap()
            .values()
            .map(|r| {
                (
                    r["type"].as_str().unwrap().to_string(),
                    r["name"].as_str().unwrap().to_string(),
                    r["content"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        v.sort();
        v
    }

    /// The label of the zone's AAAA record for `v6`, once there is one.
    async fn label_for(&self, v6: &str) -> String {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some((_, name, _)) = self
                .zone()
                .into_iter()
                .find(|(k, _, c)| k == "AAAA" && c == v6)
            {
                return name.strip_suffix(".fcvm.example").unwrap().to_string();
            }
            assert!(std::time::Instant::now() < deadline, "{:?}", self.zone());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn wait_until(&self, what: &str, f: impl Fn(&Self) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !f(self) {
            assert!(
                std::time::Instant::now() < deadline,
                "{what}: zone {:?}, tile {}",
                self.zone(),
                self.tile()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn open(&self, open: bool) {
        self.gate
            .open
            .store(open, std::sync::atomic::Ordering::SeqCst);
    }
}

/// A launch gets a label never used before; its records are written (AAAA for the slot's own
/// address, A for the host's IPv4, both unproxied with TTL 60), confirmed at the zone's
/// authoritative servers, and only then do its frame URLs reach the page. The start-up sweep
/// removes names of the fleet's shape that nothing references and leaves every other name.
#[tokio::test]
async fn a_launch_is_published_only_after_its_names_answer() {
    let fx = dns_fixture(&[
        ("CNAME", "stage.fcvm.example", "worker.example"),
        ("AAAA", "fleet-api-stage.fcvm.example", "2001:db8::99"),
        ("AAAA", "dnstest-1.fcvm.example", "2001:db8::98"),
        ("AAAA", "slot-9.fcvm.example", "2001:db8::97"),
        ("A", "slot-9-v4.fcvm.example", "192.0.2.97"),
        ("AAAA", "slot-7-abcdefgh.fcvm.example", "2001:db8::96"),
    ])
    .await;
    fx.report("running", 1, Some("2001:db8::1"), Some("192.0.2.10"));
    let label = fx.label_for("2001:db8::1").await;
    assert!(
        label.starts_with("slot-1-") && label.len() == "slot-1-".len() + 8,
        "{label}"
    );
    fx.wait_until("the A record", |fx| {
        fx.zone().contains(&(
            "A".into(),
            format!("{label}-v4.fcvm.example"),
            "192.0.2.10".into(),
        ))
    })
    .await;
    // Written, not yet answering: the page gets nothing.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(fx.tile()["phase"], "running");
    assert!(fx.tile()["frame_url"].is_null(), "{}", fx.tile());
    assert!(fx.tile()["frame_url_v4"].is_null(), "{}", fx.tile());
    fx.open(true);
    fx.wait_until("the published URLs", |fx| !fx.tile()["frame_url"].is_null())
        .await;
    assert_eq!(
        fx.tile()["frame_url"],
        format!("wss://{label}.fcvm.example/frames")
    );
    assert_eq!(
        fx.tile()["frame_url_v4"],
        format!("wss://{label}-v4.fcvm.example:20200/frames")
    );
    assert!(
        !fx.gate
            .asked_before_written
            .load(std::sync::atomic::Ordering::SeqCst),
        "a launch was confirmed before its records were written"
    );
    // No fleet record is proxied, and each has TTL 60.
    for r in fx.cf.records.lock().unwrap().values() {
        if r["name"].as_str().unwrap().starts_with("slot-") {
            assert_eq!(
                (r["proxied"].clone(), r["ttl"].clone()),
                (json!(false), json!(60)),
                "{r}"
            );
        }
    }
    // The sweep: the fleet's unreferenced names are gone, every other name stays.
    let names: Vec<String> = fx.zone().into_iter().map(|(_, n, _)| n).collect();
    for keep in [
        "stage.fcvm.example",
        "fleet-api-stage.fcvm.example",
        "dnstest-1.fcvm.example",
    ] {
        assert!(names.contains(&keep.to_string()), "{keep}: {names:?}");
    }
    for gone in [
        "slot-9.fcvm.example",
        "slot-9-v4.fcvm.example",
        "slot-7-abcdefgh.fcvm.example",
    ] {
        assert!(!names.contains(&gone.to_string()), "{gone}: {names:?}");
    }
}

/// A slot that reports another address at the same generation shows no URL until that
/// address has its own published names: its old names point at an address it left.
#[tokio::test]
async fn a_new_address_at_the_same_generation_hides_the_old_urls() {
    let fx = dns_fixture(&[]).await;
    fx.open(true);
    fx.report("running", 1, Some("2001:db8::1"), Some("192.0.2.10"));
    let first = fx.label_for("2001:db8::1").await;
    fx.wait_until("published", |fx| !fx.tile()["frame_url"].is_null())
        .await;
    fx.open(false);
    fx.report("running", 1, Some("2001:db8::5"), Some("192.0.2.10"));
    let second = fx.label_for("2001:db8::5").await;
    assert_ne!(first, second);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        fx.tile()["frame_url"].is_null() && fx.tile()["frame_url_v4"].is_null(),
        "the old names were shown for a new address: {}",
        fx.tile()
    );
    fx.open(true);
    fx.wait_until("the new address published", |fx| {
        fx.tile()["frame_url"] == format!("wss://{second}.fcvm.example/frames")
    })
    .await;
}

/// Every launch gets new names. A relaunch keeps the earlier launch's records while the new
/// launch is written and confirmed (the page shows no URL for the new launch until then),
/// and removes them only once the new names are published and nothing references the old.
#[tokio::test]
async fn each_launch_gets_new_names_and_old_ones_go_once_unreferenced() {
    let fx = dns_fixture(&[]).await;
    fx.open(true);
    fx.report("running", 1, Some("2001:db8::1"), Some("192.0.2.10"));
    let first = fx.label_for("2001:db8::1").await;
    fx.wait_until("the first launch published", |fx| {
        !fx.tile()["frame_url"].is_null()
    })
    .await;
    // The relaunch: no address yet, then the new launch's addresses, confirmation held.
    fx.open(false);
    fx.report("starting", 2, None, None);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        fx.zone().iter().any(|(_, n, _)| n.starts_with(&first)),
        "{:?}",
        fx.zone()
    );
    assert!(fx.tile()["frame_url"].is_null(), "{}", fx.tile());
    fx.report("running", 2, Some("2001:db8::2"), Some("192.0.2.11"));
    let second = fx.label_for("2001:db8::2").await;
    assert_ne!(first, second);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        fx.zone().iter().any(|(_, n, _)| n.starts_with(&first)),
        "the first launch's names went before the second's were published: {:?}",
        fx.zone()
    );
    assert!(fx.tile()["frame_url"].is_null(), "{}", fx.tile());
    fx.open(true);
    fx.wait_until("the second launch published", |fx| {
        fx.tile()["frame_url"] == format!("wss://{second}.fcvm.example/frames")
    })
    .await;
    fx.wait_until("the first launch's names removed", |fx| {
        !fx.zone().iter().any(|(_, n, _)| n.starts_with(&first))
    })
    .await;
    let w = fx.cf.writes.lock().unwrap().clone();
    assert!(
        !w.iter().any(|x| x.starts_with("update")),
        "a name changed address in place: {w:?}"
    );
}

/// An EC2 tile whose renderer reports a fit; it records the budgets posted to it.
struct BudgetEc2 {
    posted: std::sync::Mutex<Vec<fleet_core::protocol::BudgetBody>>,
}

#[async_trait::async_trait]
impl ec2::Ec2Driver for BudgetEc2 {
    async fn launch(&self, _tile: &str) -> Result<ec2::Launched, ec2::LaunchError> {
        Ok(ec2::Launched {
            instance: "i-test".into(),
            frame_url: None,
            ipv6: None,
            ipv4: None,
        })
    }
    async fn ready(&self, _tile: &str, l: &ec2::Launched) -> anyhow::Result<ec2::Launched> {
        Ok(l.clone())
    }
    async fn terminate(&self, _instance: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn post_scene(
        &self,
        _tile: &str,
        _l: &ec2::Launched,
        _body: &fleet_core::protocol::RendererScene,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn post_budget(
        &self,
        _tile: &str,
        _l: &ec2::Launched,
        body: &fleet_core::protocol::BudgetBody,
    ) -> anyhow::Result<()> {
        self.posted.lock().unwrap().push(*body);
        Ok(())
    }
    async fn health(&self, _tile: &str, _l: &ec2::Launched) -> anyhow::Result<serde_json::Value> {
        Ok(json!({"fps": 30.0, "budget_fit": 1.0e6, "t": 5.0}))
    }
}

/// The coordinator reads every tile's fit (the agents' health reports, the EC2 tile's
/// `/health`), and the slowest tile's budget reaches the agents and the EC2 tile with one
/// start time.
#[tokio::test]
async fn the_slowest_tile_sets_one_budget_for_every_tile() {
    let config = Config {
        ec2_max: 1,
        ..Config::default()
    };
    let driver = Arc::new(BudgetEc2 {
        posted: Default::default(),
    });
    let (handle, _actor) = start(
        State::new(config, now_ms()),
        None,
        Some(driver.clone() as Arc<dyn ec2::Ec2Driver>),
        None,
        None,
        None,
    );
    let mut agent = FakeAgent::connect(&handle, "gpu-a", 2);
    agent.assign().await;
    agent.report("slot-1", "running", 1);
    agent.report("slot-2", "running", 1);
    for (id, fit, t) in [("slot-1", 2.0e6, 4.0), ("slot-2", 5.0e5, 6.0)] {
        agent
            .to_coord
            .send(AgentMsg::Health(fleet_core::protocol::HealthReport {
                id: id.into(),
                fps: 30.0,
                gpu: Value::Null,
                budget_fit: Some(fit),
                t: Some(t),
                render_ms_max: None,
            }))
            .unwrap();
    }
    let got = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let CoordMsg::Budget(b) = agent.next().await {
                return b;
            }
        }
    })
    .await
    .expect("a budget within 20 s");
    assert_eq!(got.budget, fleet_core::budget::ladder_floor(5.0e5 / 1.15));
    assert_eq!(
        got.t, 7.0,
        "a second past the newest frame (6.0), on a frame"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while driver.posted.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "the EC2 tile got no budget");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(driver.posted.lock().unwrap()[0], got);
}
