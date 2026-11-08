//! Minecraft mode in the fleet agent, against a fake fcvm (tests/fixtures/fake-minecraft-fcvm.sh)
//! and fake tiles: the gold flow in its order, every server restored from it and reported up only
//! once its bots joined again, the scene posted with the tile's name, a driver reaching the tile
//! without a relaunch, a fork that snapshots its running source without stopping it, and the gold
//! snapshot reused.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use fcvm::fleet::agent::minecraft::{Minecraft, MinecraftCfg, Place};
use fcvm::fleet::agent::slot::AgentCfg;
use fleet_core::protocol::{AgentMsg, AssignMinecraft, AssignSlot, ForkFrom, SlotReport};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, watch};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

type Scenes = Arc<Mutex<Vec<(Option<String>, serde_json::Value)>>>;

#[derive(Clone)]
struct Tile {
    dir: PathBuf,
    name: String,
    scenes: Scenes,
}

impl Tile {
    fn sessions(&self) -> Option<u64> {
        std::fs::read_to_string(self.dir.join(format!("sessions-{}", self.name)))
            .ok()?
            .trim()
            .parse()
            .ok()
    }
}

async fn health(State(t): State<Tile>) -> Result<Json<serde_json::Value>, StatusCode> {
    let s = t.sessions().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    Ok(Json(
        serde_json::json!({"ready": true, "tps": 20.0, "sessions": s, "chunks": 12}),
    ))
}

async fn scene(
    State(t): State<Tile>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> StatusCode {
    if t.sessions().is_none() {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    t.scenes.lock().unwrap().push((auth, body));
    StatusCode::OK
}

async fn serve_tile(dir: &Path, name: &str) -> (SocketAddr, Scenes) {
    let scenes: Scenes = Arc::default();
    let t = Tile {
        dir: dir.to_path_buf(),
        name: name.into(),
        scenes: scenes.clone(),
    };
    let app = Router::new()
        .route("/health", get(health))
        .route("/scene", post(scene))
        .with_state(t);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, scenes)
}

fn wrapper(dir: &Path, snaps: &Path, env: &[(&str, &str)]) -> PathBuf {
    let p = dir.join("fcvm");
    let mut script = format!(
        "#!/bin/sh\nexport FAKE_FCVM_DIR='{}'\nexport FAKE_SNAPSHOT_DIR='{}'\n",
        dir.display(),
        snaps.display()
    );
    for (k, v) in env {
        script.push_str(&format!("export {k}='{v}'\n"));
    }
    script.push_str(&format!(
        "exec '{}' \"$@\"\n",
        fixture("fake-minecraft-fcvm.sh").display()
    ));
    std::fs::write(&p, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn agent_cfg(fcvm: &Path) -> AgentCfg {
    AgentCfg {
        fcvm: fcvm.to_path_buf(),
        image: String::new(),
        extra_args: vec![],
        publish_ip: "127.0.0.1".into(),
        frame_host: "mc-host.example".into(),
        ipv6_prefix: None,
        frame_zone: Some("fleet.example".into()),
        tls_publish_base: None,
        public_ipv4: None,
        place: None,
        sysfs_root: PathBuf::from("/nonexistent-sysfs"),
        start_timeout: Duration::from_secs(20),
        stop_grace: Duration::from_secs(5),
        gpu_free_timeout: Duration::from_secs(5),
        health_interval: Duration::from_millis(200),
        backoff_min: Duration::from_secs(1),
        backoff_max: Duration::from_secs(30),
        stable_after: Duration::from_secs(60),
        scrub: false,
        scrub_timeout: Duration::from_secs(10),
        quarantine_after: 3,
        phone: None,
        scene_token: fcvm::fleet::coord::scene_token::SceneToken::new(Some("tok-1".into())),
    }
}

fn mc_cfg(snaps: &Path) -> MinecraftCfg {
    MinecraftCfg {
        count: 3,
        image: "localhost/fcvm-minecraft:test".into(),
        image_id: "img-1".into(),
        server: Some("Pumpkin test".into()),
        mem_mib: 1024,
        cpus: 1,
        bots: 3,
        snapshot_dir: snaps.to_path_buf(),
        tls_dir: None,
        gold_deadline: Duration::from_secs(20),
        ready_deadline: Duration::from_secs(20),
        snapshot_deadline: Duration::from_secs(20),
        poll: Duration::from_millis(20),
        steady: 2,
    }
}

struct Fake {
    _dir: tempfile::TempDir,
    path: PathBuf,
    snaps: PathBuf,
    scenes: Vec<Scenes>,
    mc: Minecraft,
    rx: mpsc::UnboundedReceiver<AgentMsg>,
    /// Reports read while waiting for another.
    seen: Vec<SlotReport>,
    _scene: watch::Sender<Option<fleet_core::protocol::SharedScene>>,
}

impl Fake {
    fn events(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(self.path.join("events.log"))
            .unwrap_or_default()
            .lines()
            .map(|l| l.split(' ').map(String::from).collect())
            .collect()
    }

    /// The next report of `id` in `phase` at `generation`.
    async fn report(&mut self, id: &str, phase: &str, generation: u64) -> SlotReport {
        let hit = |r: &SlotReport| r.id == id && r.phase == phase && r.generation == generation;
        if let Some(i) = self.seen.iter().position(hit) {
            return self.seen.remove(i);
        }
        let end = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let m = tokio::time::timeout_at(end, self.rx.recv())
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "no {phase} report of {id} at {generation}; events {:?}",
                        self.events()
                    )
                })
                .expect("the agent's channel closed");
            if let AgentMsg::Slot(r) = m {
                assert_ne!(r.phase, "failed", "{id} failed: {:?}", r.error);
                if hit(&r) {
                    return r;
                }
                self.seen.push(r);
            }
        }
    }
}

const IDS: [&str; 3] = ["mc-1", "mc-2", "mc-3"];

async fn fake() -> Fake {
    fake_with(&[], |_| {}).await
}

/// As [`fake`], with the fake fcvm's environment and a change to the agent's configuration,
/// and with whatever `before` leaves in the snapshot directory before the agent starts.
async fn fake_with(env: &[(&str, &str)], tweak: impl FnOnce(&mut MinecraftCfg)) -> Fake {
    fake_from(env, tweak, |_| {}).await
}

async fn fake_from(
    env: &[(&str, &str)],
    tweak: impl FnOnce(&mut MinecraftCfg),
    before: impl FnOnce(&Path),
) -> Fake {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let snaps = path.join("snapshots");
    std::fs::create_dir_all(&snaps).unwrap();
    before(&snaps);
    let fcvm = wrapper(&path, &snaps, env);
    let mut places = Vec::new();
    let mut scenes = Vec::new();
    for id in IDS {
        let (addr, s) = serve_tile(&path, id).await;
        places.push(Place {
            ipv6: None,
            http: addr,
        });
        scenes.push(s);
    }
    let (gold_addr, _) = serve_tile(&path, "mc-gold").await;
    let (out, rx) = mpsc::unbounded_channel();
    let shared: fleet_core::protocol::SharedScene = serde_json::from_value(serde_json::json!({
        "epoch_unix_ms": 1, "image_aspect": 1.0, "camera": [], "time_of_day": 0.5, "words": [],
        "lights": [], "admit_keys": ["ab".repeat(32)]
    }))
    .unwrap();
    let (scene_tx, scene_rx) = watch::channel(Some(shared));
    let mut cfg = mc_cfg(&snaps);
    tweak(&mut cfg);
    let mc = Minecraft::start(
        Arc::new(cfg),
        Arc::new(agent_cfg(&fcvm)),
        String::new(),
        places,
        Place {
            ipv6: None,
            http: gold_addr,
        },
        out,
        reqwest::Client::builder().no_proxy().build().unwrap(),
        scene_rx,
    )
    .await;
    Fake {
        _dir: dir,
        path,
        snaps,
        scenes,
        mc,
        rx,
        seen: Vec::new(),
        _scene: scene_tx,
    }
}

fn assign(
    id: &str,
    index: u32,
    generation: u64,
    fork: Option<(&str, u32)>,
    driver: Option<&str>,
) -> AssignSlot {
    AssignSlot {
        id: id.into(),
        sbdf: String::new(),
        generation,
        rect: None,
        private_camera: None,
        layout_generation: 0,
        rect_from_frame: 0,
        previous_rect: None,
        standby: false,
        warm: false,
        phone: None,
        windows: None,
        minecraft: Some(AssignMinecraft {
            index,
            clicked_at: Some(1),
            fork: fork.map(|(slot, index)| ForkFrom {
                slot: slot.into(),
                index,
            }),
            driver: driver.map(String::from),
            driver_until: driver.map(|_| 99),
        }),
    }
}

#[tokio::test]
async fn the_gold_flow_runs_first_and_every_server_restores_from_it() {
    let mut f = fake().await;
    f.mc.assign(&[
        assign("mc-1", 0, 1, None, None),
        assign("mc-2", 1, 1, None, None),
    ]);
    let r1 = f.report("mc-1", "running", 1).await;
    f.report("mc-2", "running", 1).await;
    let ev = f.events();
    let kinds: Vec<(&str, &str)> = ev.iter().map(|e| (e[0].as_str(), e[1].as_str())).collect();
    assert_eq!(kinds[0], ("podman_run", "mc-gold"), "{ev:?}");
    assert_eq!(kinds[1], ("snapshot_create", "mc-gold"), "{ev:?}");
    let gold_tag = mc_cfg(&f.snaps).gold_tag();
    assert_eq!(ev[1][3], gold_tag);
    assert_eq!(
        ev[1].get(4).map(String::as_str),
        Some("stop"),
        "the gold snapshot stops its VM: {ev:?}"
    );
    // Every server is a restore of the gold snapshot, made only after it.
    for id in ["mc-1", "mc-2"] {
        let e = ev
            .iter()
            .find(|e| e[0] == "snapshot_run" && e[1] == id)
            .unwrap_or_else(|| panic!("{ev:?}"));
        assert_eq!(e[3], gold_tag);
    }
    assert!(f.snaps.join(format!("{gold_tag}.minecraft.json")).is_file());
    assert!(r1.first_chunk_at.is_some());
    for k in ["restore_ms", "first_chunk_ms", "total_ms"] {
        assert!(r1.timings.contains_key(k), "{k} in {:?}", r1.timings);
    }
    assert_eq!(
        r1.frame_url.as_deref(),
        Some("wss://mc-1.fleet.example/view")
    );
    // The tile holds its scene: its name, its generation, the admission keys, the scene token.
    let s = f.scenes[0].lock().unwrap().clone();
    let (auth, body) = s.last().expect("mc-1 got a scene");
    assert_eq!(auth.as_deref(), Some("Bearer tok-1"));
    assert_eq!(body["tile"], "mc-1");
    assert_eq!(body["generation"], 1);
    assert_eq!(body["admit_keys"][0], "ab".repeat(32));
    assert!(body["driver"].is_null());
    f.mc.stop().await;
}

#[tokio::test]
async fn a_driver_reaches_the_tile_without_a_relaunch() {
    let mut f = fake().await;
    f.mc.assign(&[assign("mc-1", 0, 1, None, None)]);
    f.report("mc-1", "running", 1).await;
    let runs = |f: &Fake| {
        f.events()
            .iter()
            .filter(|e| e[0] == "snapshot_run" && e[1] == "mc-1")
            .count()
    };
    assert_eq!(runs(&f), 1);
    f.mc.assign(&[assign("mc-1", 0, 1, None, Some("v-7"))]);
    let end = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let got = f.scenes[0]
            .lock()
            .unwrap()
            .last()
            .map(|(_, b)| b["driver"].clone());
        if got == Some(serde_json::json!("v-7")) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < end,
            "the driver never reached the tile"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        f.scenes[0].lock().unwrap().last().unwrap().1["driver_until"],
        99
    );
    assert_eq!(runs(&f), 1, "a driver relaunches nothing");
    f.mc.stop().await;
}

#[tokio::test]
async fn a_fork_copies_its_running_source_into_the_target_without_stopping_the_source() {
    let mut f = fake().await;
    f.mc.assign(&[
        assign("mc-1", 0, 1, None, None),
        assign("mc-2", 1, 1, None, None),
    ]);
    f.report("mc-1", "running", 1).await;
    f.report("mc-2", "running", 1).await;
    // The source's bots join again (say, after a restart of theirs): the copy carries that count.
    std::fs::write(f.path.join("sessions-mc-1"), "5").unwrap();
    f.mc.assign(&[
        assign("mc-1", 0, 1, None, None),
        assign("mc-2", 1, 2, Some(("mc-1", 0)), None),
    ]);
    f.report("mc-2", "forking", 2).await;
    let r = f.report("mc-2", "running", 2).await;
    let ev = f.events();
    let tag = "mc-fork-mc-2-2";
    let snap = ev
        .iter()
        .position(|e| e[0] == "snapshot_create" && e[3] == tag)
        .unwrap_or_else(|| panic!("{ev:?}"));
    assert_eq!(ev[snap][1], "mc-1", "the fork snapshots its source");
    assert_eq!(
        ev[snap].len(),
        5,
        "the fork's snapshot does not stop its source: {:?}",
        ev[snap]
    );
    // The target's old VM stops only after the snapshot, and the copy restores after that.
    let old_exit = ev
        .iter()
        .position(|e| e[0] == "exit" && e[1] == "mc-2")
        .unwrap();
    let run = ev
        .iter()
        .position(|e| e[0] == "snapshot_run" && e[1] == "mc-2" && e[3] == tag)
        .unwrap();
    assert!(snap < old_exit && old_exit < run, "{ev:?}");
    assert!(
        !ev.iter().any(|e| e[0] == "exit" && e[1] == "mc-1"),
        "the source never stops: {ev:?}"
    );
    // Up only once the copy's bots joined past the source's count.
    assert_eq!(
        std::fs::read_to_string(f.path.join("sessions-mc-2"))
            .unwrap()
            .trim(),
        "6"
    );
    assert!(r.timings.contains_key("snapshot_ms"), "{:?}", r.timings);
    assert!(
        !f.snaps.join(tag).exists(),
        "the fork's snapshot is removed once the copy runs"
    );
    f.mc.stop().await;
}

#[tokio::test]
async fn a_fork_whose_source_does_not_run_here_restores_the_gold_snapshot() {
    let mut f = fake().await;
    f.mc.assign(&[assign("mc-2", 1, 1, None, None)]);
    f.report("mc-2", "running", 1).await;
    f.mc.assign(&[assign("mc-2", 1, 2, Some(("mc-1", 0)), None)]);
    f.report("mc-2", "running", 2).await;
    let ev = f.events();
    let gold = mc_cfg(&f.snaps).gold_tag();
    assert!(
        ev.iter()
            .any(|e| e[0] == "snapshot_run" && e[1] == "mc-2" && e[3] == gold),
        "{ev:?}"
    );
    assert!(
        !ev.iter()
            .any(|e| e[0] == "snapshot_create" && e[1] != "mc-gold"),
        "{ev:?}"
    );
    f.mc.stop().await;
}

#[tokio::test]
async fn a_gold_snapshot_made_before_is_reused() {
    let mut f = fake().await;
    let tag = mc_cfg(&f.snaps).gold_tag();
    std::fs::create_dir_all(f.snaps.join(&tag)).unwrap();
    std::fs::write(f.snaps.join(&tag).join("config.json"), "{}").unwrap();
    std::fs::write(f.snaps.join(&tag).join("sessions"), "1").unwrap();
    std::fs::write(
        f.snaps.join(format!("{tag}.minecraft.json")),
        r#"{"sessions":1}"#,
    )
    .unwrap();
    f.mc.assign(&[assign("mc-1", 0, 1, None, None)]);
    f.report("mc-1", "running", 1).await;
    let ev = f.events();
    assert!(!ev.iter().any(|e| e[1] == "mc-gold"), "no gold VM: {ev:?}");
    f.mc.stop().await;
}

#[tokio::test]
async fn stopping_the_agent_stops_every_server() {
    let mut f = fake().await;
    f.mc.assign(&[
        assign("mc-1", 0, 1, None, None),
        assign("mc-3", 2, 1, None, None),
    ]);
    f.report("mc-1", "running", 1).await;
    f.report("mc-3", "running", 1).await;
    let path = f.path.clone();
    f.mc.stop().await;
    let ev = std::fs::read_to_string(path.join("events.log")).unwrap();
    for id in ["mc-1", "mc-3"] {
        assert!(
            ev.lines().any(|l| l.starts_with(&format!("exit {id} "))),
            "{id} stopped: {ev}"
        );
    }
}

/// A restore runs the tile's restore hook, which drops the scene the VM was copied with, so a
/// fork's target never serves its source's borrower before the agent posts its own scene.
#[tokio::test]
async fn every_restore_runs_the_tiles_restore_hook() {
    let mut f = fake().await;
    f.mc.assign(&[assign("mc-1", 0, 1, None, None)]);
    f.report("mc-1", "running", 1).await;
    let args = std::fs::read_to_string(f.path.join("args-mc-1")).unwrap();
    let args: Vec<&str> = args.lines().collect();
    let i = args
        .iter()
        .position(|a| *a == "--on-restore")
        .expect("no restore hook");
    assert_eq!(args[i + 1], "mctile restored");
    f.mc.stop().await;
}

/// A fork whose copy cannot run is tried once; the next attempt restores the gold snapshot
/// rather than snapshotting the source again, and the copy's snapshot is removed.
#[tokio::test]
async fn a_failed_copy_falls_back_to_the_gold_snapshot() {
    let mut f = fake_with(&[("FAKE_FAIL_FORK", "1")], |_| {}).await;
    f.mc.assign(&[
        assign("mc-1", 0, 1, None, None),
        assign("mc-2", 1, 1, None, None),
    ]);
    f.report("mc-1", "running", 1).await;
    f.report("mc-2", "running", 1).await;
    f.mc.assign(&[
        assign("mc-1", 0, 1, None, None),
        assign("mc-2", 1, 2, Some(("mc-1", 0)), None),
    ]);
    let end = tokio::time::Instant::now() + Duration::from_secs(30);
    let r = loop {
        let m = tokio::time::timeout_at(end, f.rx.recv())
            .await
            .expect("no report")
            .unwrap();
        if let AgentMsg::Slot(r) = m {
            if r.id == "mc-2" && r.phase == "running" && r.generation == 2 {
                break r;
            }
        }
    };
    assert!(!r.timings.contains_key("snapshot_ms"), "{:?}", r.timings);
    let ev = f.events();
    let snaps: Vec<_> = ev
        .iter()
        .filter(|e| e[0] == "snapshot_create" && e[1] == "mc-1")
        .collect();
    assert_eq!(snaps.len(), 1, "the source is snapshotted once: {ev:?}");
    let gold = mc_cfg(&f.snaps).gold_tag();
    assert!(
        ev.iter()
            .any(|e| e[0] == "snapshot_run" && e[1] == "mc-2" && e[3] == gold),
        "{ev:?}"
    );
    assert!(
        !f.snaps.join("mc-fork-mc-2-2").exists(),
        "the failed copy's snapshot is left"
    );
    f.mc.stop().await;
}

/// A copy whose restore stalls holds its snapshot's generation lock until it is stopped: the
/// launch stops it at its ready deadline before it removes that snapshot, so the deadline still
/// ends the launch, and the server is relaunched from the gold snapshot.
#[tokio::test]
async fn a_stalled_copy_is_stopped_before_its_snapshot_is_removed() {
    let mut f = fake_with(&[("FAKE_STALL_FORK", "1")], |c| {
        c.ready_deadline = Duration::from_secs(2)
    })
    .await;
    f.mc.assign(&[
        assign("mc-1", 0, 1, None, None),
        assign("mc-2", 1, 1, None, None),
    ]);
    f.report("mc-1", "running", 1).await;
    f.report("mc-2", "running", 1).await;
    f.mc.assign(&[
        assign("mc-1", 0, 1, None, None),
        assign("mc-2", 1, 2, Some(("mc-1", 0)), None),
    ]);
    let end = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let m = tokio::time::timeout_at(end, f.rx.recv())
            .await
            .expect("the stalled copy's launch never ended")
            .unwrap();
        if let AgentMsg::Slot(r) = m {
            if r.id == "mc-2" && r.generation == 2 && r.phase == "running" {
                break;
            }
        }
    }
    let ev = f.events();
    let stalled = ev
        .iter()
        .position(|e| e[0] == "stalled" && e[1] == "mc-2")
        .unwrap_or_else(|| panic!("{ev:?}"));
    assert!(
        ev[stalled..]
            .iter()
            .any(|e| e[0] == "exit" && e[2] == ev[stalled][2]),
        "the stalled copy was not stopped: {ev:?}"
    );
    assert!(!f.snaps.join("mc-fork-mc-2-2").exists());
    f.mc.stop().await;
}

/// A snapshot create that outlives its deadline is never killed: killed after it paused the
/// source, it would leave that server paused for good. It runs to its end.
#[tokio::test]
async fn a_slow_fork_snapshot_is_never_killed() {
    let mut f = fake_with(&[], |c| c.snapshot_deadline = Duration::from_millis(500)).await;
    f.mc.assign(&[
        assign("mc-1", 0, 1, None, None),
        assign("mc-2", 1, 1, None, None),
    ]);
    f.report("mc-1", "running", 1).await;
    f.report("mc-2", "running", 1).await;
    // Only now: the gold flow's own snapshot runs at full speed.
    std::fs::write(
        f.path.join("fcvm"),
        std::fs::read_to_string(f.path.join("fcvm"))
            .unwrap()
            .replace("exec '", "export FAKE_SNAPSHOT_SECS=2\nexec '"),
    )
    .unwrap();
    f.mc.assign(&[
        assign("mc-1", 0, 1, None, None),
        assign("mc-2", 1, 2, Some(("mc-1", 0)), None),
    ]);
    let end = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if f.events()
            .iter()
            .any(|e| e[0] == "snapshot_done" && e[1] == "mc-1")
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < end,
            "the snapshot create was killed: {:?}",
            f.events()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    f.mc.stop().await;
}

/// A fork snapshot an earlier agent left is removed when the agent starts.
#[tokio::test]
async fn fork_snapshots_left_by_an_earlier_agent_are_removed_at_start() {
    let f = fake_from(
        &[],
        |_| {},
        |snaps| {
            std::fs::create_dir_all(snaps.join("mc-fork-mc-2-7/disk")).unwrap();
            std::fs::write(snaps.join("mc-fork-mc-2-7.lock"), "").unwrap();
            std::fs::create_dir_all(snaps.join("mc-gold-keep")).unwrap();
        },
    )
    .await;
    assert!(!f.snaps.join("mc-fork-mc-2-7").exists());
    assert!(!f.snaps.join("mc-fork-mc-2-7.lock").exists());
    assert!(
        f.snaps.join("mc-gold-keep").exists(),
        "only fork snapshots are swept"
    );
    f.mc.stop().await;
}
