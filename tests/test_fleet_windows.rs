//! Windows mode in the fleet agent, against a fake fcvm (tests/fixtures/fake-windows-fcvm.sh),
//! a fake podman (tests/fixtures/fake-windows-podman.sh) and fake bridges: the gold flow in
//! its order, every sandbox restored from it, the timings and the desktop time reported, an
//! action posted to the bridge once, a reset relaunching, the gold snapshot reused, and an
//! expired image refused.

mod fleet_support;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use fcvm::fleet::agent::slot::AgentCfg;
use fcvm::fleet::agent::windows::{Manifest, WindowsCfg};
use fcvm::fleet::agent::{serve, Agent};
use fcvm::fleet::lines::{spawn_writer, LineReader};
use fleet_core::protocol::{AgentMsg, AssignSlot, AssignWindows, CoordMsg, SlotReport};
use fleet_core::windows::{Gold, WindowsAction, WindowsActionReq, WindowsImage};
use fleet_support::consecutive_listeners;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::mpsc;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

type Posted = Arc<Mutex<Vec<(Option<String>, serde_json::Value)>>>;

#[derive(Clone)]
struct Bridge {
    marker: PathBuf,
    actions: Posted,
}

async fn health(State(b): State<Bridge>) -> Result<Json<serde_json::Value>, StatusCode> {
    let up: u64 = std::fs::read_to_string(&b.marker)
        .ok()
        .and_then(|t| t.trim().parse().ok())
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    Ok(Json(
        serde_json::json!({"fps": 30.0, "windows": {"state": "desktop", "times_unix_ms": {
        "start": up - 100, "connect_started": up - 90, "tcp": up - 80, "rdp": up - 50,
        "first_frame": up - 20, "desktop": up}}}),
    ))
}

async fn action(
    State(b): State<Bridge>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> StatusCode {
    if !b.marker.exists() {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    b.actions.lock().unwrap().push((auth, body));
    StatusCode::OK
}

struct Fake {
    dir: tempfile::TempDir,
    fcvm: PathBuf,
    podman: PathBuf,
    base: u16,
    actions: Vec<Posted>,
}

fn wrapper(dir: &Path, name: &str, target: &Path, env: &[(&str, String)]) -> PathBuf {
    let mut script = format!("#!/bin/sh\nexport FAKE_FCVM_DIR='{}'\n", dir.display());
    for (k, v) in env {
        script.push_str(&format!("export {k}='{v}'\n"));
    }
    script.push_str(&format!("exec '{}' \"$@\"\n", target.display()));
    let p = dir.join(name);
    std::fs::write(&p, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

/// A temp dir with an image directory, a snapshot directory, wrappers for the fake fcvm and
/// podman, and `n` fake bridges on consecutive ports.
fn fake(n: usize) -> Fake {
    let dir = tempfile::tempdir().unwrap();
    let snaps = dir.path().join("snapshots");
    std::fs::create_dir_all(&snaps).unwrap();
    let env = [("FAKE_SNAPSHOT_DIR", snaps.display().to_string())];
    let fcvm = wrapper(dir.path(), "fcvm", &fixture("fake-windows-fcvm.sh"), &env);
    let podman = wrapper(
        dir.path(),
        "podman",
        &fixture("fake-windows-podman.sh"),
        &env,
    );
    let (base, listeners) = consecutive_listeners(n);
    let mut actions = Vec::new();
    for (i, l) in listeners.into_iter().enumerate() {
        l.set_nonblocking(true).unwrap();
        let b = Bridge {
            marker: dir.path().join(format!("bridge-{}.up", base + i as u16)),
            actions: Arc::new(Mutex::new(Vec::new())),
        };
        actions.push(b.actions.clone());
        let app = Router::new()
            .route("/health", get(health))
            .route("/action", post(action))
            .with_state(b);
        let listener = tokio::net::TcpListener::from_std(l).unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    }
    Fake {
        dir,
        fcvm,
        podman,
        base,
        actions,
    }
}

impl Fake {
    fn image(&self, installed_at: &str, eval_expires: &str) -> PathBuf {
        let img = self.dir.path().join("image-3f9c0a1b2c4d");
        std::fs::create_dir_all(&img).unwrap();
        for f in ["disk.raw", "CLOUDHV.fd"] {
            std::fs::write(img.join(f), "").unwrap();
        }
        std::fs::write(img.join("sandbox-password"), "Sbnotused!7").unwrap();
        std::fs::write(
            img.join("manifest.json"),
            serde_json::json!({"id": "3f9c0a1b2c4d", "edition": "Windows Server 2025 Datacenter Evaluation (Desktop Experience)",
                "build": "26100.1742", "installed_at": installed_at, "eval_expires": eval_expires})
            .to_string(),
        )
        .unwrap();
        img
    }

    fn windows(&self, count: u32, img: PathBuf) -> WindowsCfg {
        WindowsCfg {
            count,
            manifest: Manifest::read(&img).unwrap(),
            image_dir: img,
            bridge_image: "localhost/fcvm-windows-bridge:test".into(),
            podman: self.podman.clone(),
            state_dir: self.dir.path().join("state"),
            snapshot_dir: self.dir.path().join("snapshots"),
            proc_root: "/proc".into(),
            idle_cpu: 0.15,
            // The fake gold VM is a shell asleep; a second of quiet is enough to say so.
            idle_for: Duration::from_secs(1),
            gold_healthy_deadline: Duration::from_secs(20),
            logon_deadline: Duration::from_secs(20),
            idle_deadline: Duration::from_secs(20),
            snapshot_deadline: Duration::from_secs(20),
            start_timeout: Duration::from_secs(20),
            poll: Duration::from_millis(50),
            tls_dir: None,
        }
    }

    fn log(&self) -> Vec<(String, String)> {
        std::fs::read_to_string(self.dir.path().join("events.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let mut p = l.split_whitespace();
                Some((p.next()?.to_string(), p.next()?.to_string()))
            })
            .collect()
    }

    fn args(&self, name: &str) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join(format!("args-{name}")))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

fn agent_cfg(fcvm: &Path) -> AgentCfg {
    AgentCfg {
        fcvm: fcvm.to_path_buf(),
        image: String::new(),
        extra_args: vec![],
        publish_ip: "127.0.0.1".into(),
        frame_host: "win-host.example".into(),
        ipv6_prefix: None,
        frame_zone: None,
        tls_publish_base: None,
        public_ipv4: None,
        place: None,
        sysfs_root: PathBuf::from("/nonexistent-sysfs"),
        start_timeout: Duration::from_secs(20),
        stop_grace: Duration::from_secs(10),
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

/// One sandbox of an assignment: id, index on the host, generation, click time, actions.
type Sandbox<'a> = (&'a str, u32, u64, Option<u64>, Vec<WindowsActionReq>);

struct Coord {
    to_agent: mpsc::UnboundedSender<CoordMsg>,
    from_agent: LineReader<ReadHalf<DuplexStream>>,
    agent: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

impl Coord {
    fn start(f: &Fake, windows: WindowsCfg) -> Coord {
        let (coord_side, agent_side) = tokio::io::duplex(1 << 20);
        let (agent_in, agent_out): (ReadHalf<DuplexStream>, WriteHalf<DuplexStream>) =
            tokio::io::split(agent_side);
        let (coord_in, coord_out) = tokio::io::split(coord_side);
        let agent = Agent {
            host: "win-host".into(),
            gpus: vec![],
            phones: vec![],
            capacity: 0,
            base_port: f.base,
            cfg: agent_cfg(&f.fcvm),
            spot_notice_url: None,
            spot_poll: Duration::from_secs(5),
            aggregate: None,
            store_ready_file: None,
            boot: Default::default(),
            potato: None,
            windows: Some(windows),
        };
        let task = tokio::spawn(serve(agent, agent_in, agent_out));
        let (to_agent, _w) = spawn_writer::<CoordMsg, _>(coord_out, "agent");
        Coord {
            to_agent,
            from_agent: LineReader::new(coord_in, "agent"),
            agent: Some(task),
        }
    }

    /// The coordinator's assignment of sandboxes `(id, index, generation, clicked_at,
    /// actions)`.
    fn assign(&self, list: &[Sandbox]) {
        let slots = list
            .iter()
            .map(|(id, index, generation, clicked_at, actions)| AssignSlot {
                id: id.to_string(),
                sbdf: String::new(),
                generation: *generation,
                rect: None,
                private_camera: None,
                layout_generation: 0,
                rect_from_frame: 0,
                previous_rect: None,
                standby: false,
                warm: false,
                phone: None,
                windows: Some(AssignWindows {
                    index: *index,
                    clicked_at: *clicked_at,
                    actions: actions.clone(),
                }),
            })
            .collect();
        self.to_agent
            .send(CoordMsg::Assign {
                slots,
                region: None,
            })
            .unwrap();
    }

    async fn next(&mut self) -> AgentMsg {
        tokio::time::timeout(Duration::from_secs(60), self.from_agent.next::<AgentMsg>())
            .await
            .expect("agent message within 60 s")
            .unwrap()
            .expect("agent connection open")
    }

    /// Read until every sandbox in `ids` reported `phase` at `generation`; also returns the
    /// image reports read meanwhile.
    async fn until(
        &mut self,
        ids: &[&str],
        phase: &str,
        generation: u64,
    ) -> (BTreeMap<String, SlotReport>, Vec<WindowsImage>) {
        let mut got = BTreeMap::new();
        let mut images = Vec::new();
        while got.len() < ids.len() {
            match self.next().await {
                AgentMsg::Slot(r)
                    if ids.contains(&r.id.as_str())
                        && r.phase == phase
                        && r.generation == generation =>
                {
                    got.insert(r.id.clone(), r);
                }
                AgentMsg::WindowsImage(w) => images.push(w),
                _ => {}
            }
        }
        (got, images)
    }

    async fn close(mut self) {
        let task = self.agent.take().unwrap();
        drop(self.to_agent);
        tokio::time::timeout(Duration::from_secs(30), task)
            .await
            .expect("agent returns after its input closes")
            .unwrap()
            .unwrap();
    }
}

fn recent() -> (String, String) {
    let now = chrono::Utc::now();
    let installed = now - chrono::Duration::days(12);
    (
        installed.to_rfc3339(),
        (installed + chrono::Duration::days(180)).to_rfc3339(),
    )
}

#[tokio::test]
async fn the_gold_flow_runs_in_order_and_every_sandbox_restores_from_it() {
    let f = fake(2);
    let (installed, expires) = recent();
    let img = f.image(&installed, &expires);
    let mut c = Coord::start(&f, f.windows(2, img.clone()));
    match c.next().await {
        AgentMsg::Hello {
            windows,
            windows_actions,
            gpus,
            ..
        } => {
            assert_eq!(windows, 2);
            assert!(gpus.is_empty());
            assert_eq!(
                WindowsAction::offered(&windows_actions),
                WindowsAction::ALL.to_vec()
            );
            assert!(!windows_actions.iter().any(|a| a == "reset"));
        }
        other => panic!("first message must be hello, got {other:?}"),
    }
    c.assign(&[
        ("win-1", 0, 1, None, vec![]),
        ("win-2", 1, 1, Some(1), vec![]),
    ]);
    let (running, images) = c.until(&["win-1", "win-2"], "running", 1).await;
    assert_eq!(images.first().map(|i| i.gold), Some(Gold::Building));
    let last = images.last().unwrap();
    assert_eq!(
        (
            last.gold,
            last.age_days,
            last.warning.as_ref(),
            last.refused.as_ref()
        ),
        (Gold::Ready, 12, None, None)
    );
    for (i, (id, r)) in running.iter().enumerate() {
        assert_eq!(r.vm.as_deref(), Some(id.as_str()));
        assert_eq!(
            r.frame_url.as_deref(),
            Some(format!("ws://win-host.example:{}/frames", f.base + i as u16).as_str())
        );
        let marker: u64 = std::fs::read_to_string(
            f.dir
                .path()
                .join(format!("bridge-{}.up", f.base + i as u16)),
        )
        .unwrap()
        .trim()
        .parse()
        .unwrap();
        assert_eq!(r.desktop_at, Some(marker), "the bridge's desktop time");
        for k in [
            "teardown_ms",
            "restore_ms",
            "connect_ms",
            "first_frame_ms",
            "desktop_ms",
            "total_ms",
        ] {
            assert!(r.timings.contains_key(k), "{id} lacks {k}: {:?}", r.timings);
        }
        assert_eq!(r.timings["first_frame_ms"], 30);
        assert_eq!(r.timings["desktop_ms"], 20);
    }
    assert!(
        running["win-2"].timings.contains_key("click_to_desktop_ms"),
        "a clicked launch is timed from the click"
    );
    assert!(!running["win-1"].timings.contains_key("click_to_desktop_ms"));
    // The order: cold boot, logon, snapshot, then both restores.
    let log = f.log();
    let at = |kind: &str, name: &str| {
        log.iter()
            .position(|(k, n)| k == kind && n == name)
            .unwrap_or_else(|| panic!("no {kind} {name} in {log:?}"))
    };
    let tag = "win-gold-3f9c0a1b2c4d";
    assert!(at("vm_run", "win-gold") < at("podman_logon", "fleet-win-gold-bridge"));
    assert!(at("podman_logon", "fleet-win-gold-bridge") < at("snapshot_create", tag));
    assert!(at("snapshot_create", tag) < at("exit", "win-gold"));
    for id in ["win-1", "win-2"] {
        assert!(at("snapshot_create", tag) < at("snapshot_run", id));
        assert!(at("snapshot_run", id) < at("podman_serve", &format!("fleet-{id}-bridge")));
    }
    assert_eq!(
        f.args("win-1"),
        ["snapshot", "run", "--snapshot", tag, "--name", "win-1"]
    );
    assert_eq!(f.args("win-gold")[..4], ["vm", "run", "--name", "win-gold"]);
    let bridge = f.args("fleet-win-2-bridge").join(" ");
    assert!(
        bridge.contains(&format!("--publish 127.0.0.1:{}:8080", f.base + 1)),
        "{bridge}"
    );
    assert!(bridge.contains(&format!(
        "{}:/run/windows/sandbox-password:ro",
        img.join("sandbox-password").display()
    )));
    assert!(
        !bridge.contains("Sbnotused"),
        "the password stays in its file"
    );

    // An action for the running launch reaches the bridge once, with the scene token; one for
    // an older launch, and `reset`, never do.
    let req = |seq, action, generation| WindowsActionReq {
        seq,
        action,
        at: 1,
        generation,
    };
    let acts = vec![
        req(1, WindowsAction::Notepad, 1),
        req(2, WindowsAction::About, 0),
        req(3, WindowsAction::Reset, 1),
    ];
    c.assign(&[
        ("win-1", 0, 1, None, acts.clone()),
        ("win-2", 1, 1, Some(1), vec![]),
    ]);
    // The same assignment again changes nothing.
    c.assign(&[
        ("win-1", 0, 1, None, acts),
        ("win-2", 1, 1, Some(1), vec![]),
    ]);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while f.actions[0].lock().unwrap().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "no action reached the bridge"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        *f.actions[0].lock().unwrap(),
        vec![(
            Some("Bearer tok-1".to_string()),
            serde_json::json!({"name": "notepad"})
        )]
    );
    assert!(f.actions[1].lock().unwrap().is_empty());

    // A reset: the sandbox's VM and bridge are torn down before it restores again.
    c.assign(&[
        ("win-1", 0, 2, Some(5), vec![]),
        ("win-2", 1, 1, Some(1), vec![]),
    ]);
    let (r, _) = c.until(&["win-1"], "running", 2).await;
    assert!(r["win-1"].timings.contains_key("total_ms"));
    let log = f.log();
    let runs: Vec<usize> = log
        .iter()
        .enumerate()
        .filter(|(_, (k, n))| k == "snapshot_run" && n == "win-1")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(runs.len(), 2, "{log:?}");
    let exits: Vec<usize> = log
        .iter()
        .enumerate()
        .filter(|(_, (k, n))| k == "exit" && (n == "win-1" || n == "fleet-win-1-bridge"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(exits.len(), 2, "the old VM and bridge exited: {log:?}");
    assert!(exits.iter().all(|e| *e < runs[1]), "{log:?}");
    assert_eq!(
        log.iter().filter(|(k, _)| k == "vm_run").count(),
        1,
        "one gold boot"
    );

    c.close().await;
    // Every VM and bridge is gone with the agent.
    let log = f.log();
    for name in ["win-1", "win-2", "fleet-win-1-bridge", "fleet-win-2-bridge"] {
        let starts = log
            .iter()
            .filter(|(k, n)| k != "exit" && n == name && !k.starts_with("snapshot_create"))
            .count();
        let exits = log.iter().filter(|(k, n)| k == "exit" && n == name).count();
        assert_eq!(starts, exits, "{name}: {log:?}");
    }
    assert!(std::fs::read_dir(f.dir.path().join("ls"))
        .unwrap()
        .next()
        .is_none());
}

#[tokio::test]
async fn a_gold_snapshot_made_before_is_reused() {
    let f = fake(1);
    let (installed, expires) = recent();
    let img = f.image(&installed, &expires);
    let tag_dir = f.dir.path().join("snapshots/win-gold-3f9c0a1b2c4d");
    std::fs::create_dir_all(&tag_dir).unwrap();
    std::fs::write(tag_dir.join("config.json"), "{}").unwrap();
    let r = f.dir.path().join("state/3f9c0a1b2c4d/desktop-ref.ppm");
    std::fs::create_dir_all(r.parent().unwrap()).unwrap();
    std::fs::write(&r, "P6\n").unwrap();
    let mut c = Coord::start(&f, f.windows(1, img));
    c.assign(&[("win-1", 0, 1, None, vec![])]);
    c.until(&["win-1"], "running", 1).await;
    let log = f.log();
    assert!(
        !log.iter()
            .any(|(k, _)| k == "vm_run" || k == "podman_logon"),
        "{log:?}"
    );
    c.close().await;
}

#[tokio::test]
async fn an_image_170_days_old_starts_no_sandbox() {
    let f = fake(1);
    let now = chrono::Utc::now();
    let installed = now - chrono::Duration::days(171);
    let img = f.image(
        &installed.to_rfc3339(),
        &(installed + chrono::Duration::days(180)).to_rfc3339(),
    );
    let tag_dir = f.dir.path().join("snapshots/win-gold-3f9c0a1b2c4d");
    std::fs::create_dir_all(&tag_dir).unwrap();
    std::fs::write(tag_dir.join("config.json"), "{}").unwrap();
    let r = f.dir.path().join("state/3f9c0a1b2c4d/desktop-ref.ppm");
    std::fs::create_dir_all(r.parent().unwrap()).unwrap();
    std::fs::write(&r, "P6\n").unwrap();
    let mut c = Coord::start(&f, f.windows(1, img));
    c.assign(&[("win-1", 0, 1, None, vec![])]);
    let (failed, images) = c.until(&["win-1"], "failed", 1).await;
    let e = failed["win-1"].error.clone().unwrap();
    assert!(e.contains("171 days old"), "{e}");
    assert!(e.contains(&installed.format("%Y-%m-%d").to_string()), "{e}");
    assert_eq!(failed["win-1"].retry_in_ms, None);
    let image = images.last().unwrap();
    assert!(
        image.refused.is_some() && image.warning.is_some(),
        "{image:?}"
    );
    assert!(!f.log().iter().any(|(k, _)| k == "snapshot_run"));
    c.close().await;
}
