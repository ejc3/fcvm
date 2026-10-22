//! The fleet agent against a fake fcvm (tests/fixtures/fake-fcvm.sh) and fake renderers:
//! reconcile, parallel relaunch, teardown before relaunch, backoff, and VMs dying with
//! the agent.

mod fleet_support;

use fcvm::fleet::agent::slot::AgentCfg;
use fcvm::fleet::agent::{serve, Agent};
use fcvm::fleet::lines::{spawn_writer, LineReader};
use fleet_core::protocol::{AgentMsg, AssignSlot, CoordMsg, SharedScene, SlotReport};
use fleet_core::scene::Scene;
use fleet_core::state::Gpu;
use fleet_core::tiling::Rect;
use fleet_support::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::{DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::mpsc;

struct Fake {
    dir: tempfile::TempDir,
    fcvm: PathBuf,
    base: u16,
    scenes: Scenes,
}

/// A temp dir, a wrapper that runs the fake fcvm with `env`, and `gpus` fake renderers.
fn fake(gpus: usize, env: &[(&str, &str)]) -> Fake {
    let dir = tempfile::tempdir().unwrap();
    let mut script = format!(
        "#!/bin/sh\nexport FAKE_FCVM_DIR='{}'\n",
        dir.path().display()
    );
    for (k, v) in env {
        script.push_str(&format!("export {k}='{v}'\n"));
    }
    script.push_str(&format!("exec '{}' \"$@\"\n", fake_fcvm().display()));
    let fcvm = dir.path().join("fcvm");
    std::fs::write(&fcvm, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&fcvm, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (base, listeners) = consecutive_listeners(gpus);
    let scenes = start_renderers(dir.path(), base, listeners);
    Fake {
        dir,
        fcvm,
        base,
        scenes,
    }
}

fn gpus(n: usize) -> Vec<Gpu> {
    (0..n)
        .map(|i| Gpu {
            sbdf: format!("0000:{:02x}:00.0", i + 1),
            name: "Tesla T4".into(),
        })
        .collect()
}

fn cfg(fcvm: &Path) -> AgentCfg {
    AgentCfg {
        fcvm: fcvm.to_path_buf(),
        image: "localhost/renderer".into(),
        extra_args: vec![],
        publish_ip: "127.0.0.1".into(),
        frame_host: "gpu-host.example".into(),
        sysfs_root: PathBuf::from("/nonexistent-sysfs"),
        start_timeout: Duration::from_secs(20),
        stop_grace: Duration::from_secs(10),
        gpu_free_timeout: Duration::from_secs(5),
        health_interval: Duration::from_millis(200),
        backoff_min: Duration::from_secs(1),
        backoff_max: Duration::from_secs(30),
        stable_after: Duration::from_secs(60),
    }
}

/// The test plays the coordinator over an in-memory pipe.
struct Coord {
    to_agent: mpsc::UnboundedSender<CoordMsg>,
    from_agent: LineReader<ReadHalf<DuplexStream>>,
    agent: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
    seen: Vec<AgentMsg>,
}

impl Coord {
    fn start(f: &Fake, n: usize, cfg: AgentCfg) -> Coord {
        Self::start_with(f, n, cfg, None)
    }

    fn start_with(f: &Fake, n: usize, cfg: AgentCfg, spot_notice_url: Option<String>) -> Coord {
        let (coord_side, agent_side) = tokio::io::duplex(1 << 20);
        let (agent_in, agent_out): (ReadHalf<DuplexStream>, WriteHalf<DuplexStream>) =
            tokio::io::split(agent_side);
        let (coord_in, coord_out) = tokio::io::split(coord_side);
        let agent = Agent {
            host: "gpu-host".into(),
            gpus: gpus(n),
            capacity: n,
            base_port: f.base,
            cfg,
            spot_notice_url,
            spot_poll: Duration::from_millis(50),
            aggregate: None,
        };
        let task = tokio::spawn(serve(agent, agent_in, agent_out));
        let (to_agent, _w) = spawn_writer::<CoordMsg, _>(coord_out, "agent");
        Coord {
            to_agent,
            from_agent: LineReader::new(coord_in, "agent"),
            agent: Some(task),
            seen: Vec::new(),
        }
    }

    fn send(&self, m: CoordMsg) {
        self.to_agent.send(m).unwrap();
    }

    fn assign(&self, slots: &[(&str, usize, u64, Option<Rect>)]) {
        self.send(CoordMsg::Assign {
            slots: slots
                .iter()
                .map(|(id, gpu, generation, rect)| AssignSlot {
                    id: id.to_string(),
                    sbdf: format!("0000:{:02x}:00.0", gpu + 1),
                    generation: *generation,
                    rect: *rect,
                })
                .collect(),
            region: None,
        });
    }

    async fn next(&mut self) -> AgentMsg {
        let m = tokio::time::timeout(Duration::from_secs(30), self.from_agent.next::<AgentMsg>())
            .await
            .expect("agent message within 30 s")
            .unwrap()
            .expect("agent connection open");
        self.seen.push(m.clone());
        m
    }

    /// Read until every slot in `ids` reported `phase` at `generation`; returns those
    /// reports.
    async fn until(
        &mut self,
        ids: &[&str],
        phase: &str,
        generation: u64,
    ) -> BTreeMap<String, SlotReport> {
        let mut got = BTreeMap::new();
        while got.len() < ids.len() {
            if let AgentMsg::Slot(r) = self.next().await {
                if ids.contains(&r.id.as_str()) && r.phase == phase && r.generation == generation {
                    got.insert(r.id.clone(), r);
                }
            }
        }
        got
    }

    /// Close the agent's input and wait for it to stop every VM and return.
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

fn rect(x: f64) -> Option<Rect> {
    Some(Rect {
        x,
        y: 0.0,
        w: 0.25,
        h: 1.0,
    })
}

fn scene() -> CoordMsg {
    CoordMsg::Scene(SharedScene::new(&Scene::new(1_000), 32.0 / 9.0))
}

#[tokio::test]
async fn reconcile_starts_every_assigned_slot_and_reports_its_timings() {
    let f = fake(2, &[]);
    let mut c = Coord::start(&f, 2, cfg(&f.fcvm));
    match c.next().await {
        AgentMsg::Hello {
            host,
            gpus,
            capacity,
            ..
        } => {
            assert_eq!(host, "gpu-host");
            assert_eq!(gpus.len(), 2);
            assert_eq!(capacity, 2);
        }
        other => panic!("first message must be hello, got {other:?}"),
    }
    c.send(scene());
    c.assign(&[("slot-1", 0, 1, rect(0.0)), ("slot-2", 1, 1, rect(0.25))]);
    let running = c.until(&["slot-1", "slot-2"], "running", 1).await;
    for (i, (id, r)) in running.iter().enumerate() {
        assert_eq!(r.vm.as_deref(), Some(format!("fleet-{id}").as_str()));
        assert_eq!(
            r.frame_url.as_deref(),
            Some(format!("ws://gpu-host.example:{}/frames", f.base + i as u16).as_str())
        );
        for key in [
            "renderer_gpu_ready_ms",
            "renderer_first_frame_ms",
            "teardown_ms",
            "restore_ms",
            "ack_ms",
            "gpu_ms",
            "container_ms",
            "first_frame_ms",
            "total_ms",
        ] {
            assert!(
                r.timings.contains_key(key),
                "{id} lacks {key}: {:?}",
                r.timings
            );
        }
        assert_eq!(r.timings["ack_ms"], 300);
        assert_eq!(r.timings["gpu_ms"], 250);
        assert!(r.timings["restore_ms"] >= 200, "{:?}", r.timings);
    }
    // Phases arrive in order for each slot.
    for id in ["slot-1", "slot-2"] {
        let phases: Vec<String> = c
            .seen
            .iter()
            .filter_map(|m| match m {
                AgentMsg::Slot(r) if r.id == id => Some(r.phase.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(phases, vec!["restoring", "starting", "running"], "{id}");
    }
    // Each renderer got its own rectangle with the shared scene.
    for (i, x) in [0.0, 0.25].iter().enumerate() {
        let posted = f.scenes[i].lock().unwrap().clone();
        assert_eq!(posted.len(), 1, "renderer {i}: {posted:?}");
        assert_eq!(posted[0]["rect"]["x"], *x);
        assert_eq!(posted[0]["epoch_unix_ms"], 1_000);
    }
    // Health reports follow.
    loop {
        if let AgentMsg::Health(h) = c.next().await {
            assert_eq!(h.fps, 30.0);
            assert_eq!(h.gpu["name"], "fake");
            break;
        }
    }
    c.close().await;
}

#[tokio::test]
async fn relaunch_tears_down_first_and_all_slots_relaunch_in_parallel() {
    let f = fake(3, &[("FAKE_STOP_SECS", "0.6")]);
    let mut c = Coord::start(&f, 3, cfg(&f.fcvm));
    c.next().await;
    c.send(scene());
    let ids = ["slot-1", "slot-2", "slot-3"];
    c.assign(&[
        ("slot-1", 0, 1, rect(0.0)),
        ("slot-2", 1, 1, rect(0.25)),
        ("slot-3", 2, 1, None),
    ]);
    c.until(&ids, "running", 1).await;
    let clicked = Instant::now();
    c.assign(&[
        ("slot-1", 0, 2, rect(0.0)),
        ("slot-2", 1, 2, rect(0.25)),
        ("slot-3", 2, 2, None),
    ]);
    let running = c.until(&ids, "running", 2).await;
    let all_back = clicked.elapsed();

    let ev = events(f.dir.path());
    let mut exits = Vec::new();
    for id in ids {
        let name = format!("fleet-{id}");
        let starts: Vec<_> = ev
            .iter()
            .filter(|e| e.kind == "start" && e.name == name)
            .collect();
        let exit: Vec<_> = ev
            .iter()
            .filter(|e| e.kind == "exit" && e.name == name)
            .collect();
        assert_eq!(starts.len(), 2, "{name}: {ev:?}");
        assert_eq!(exit.len(), 1, "{name}: {ev:?}");
        // The old VM's process exited before the new one started.
        assert_eq!(exit[0].pid, starts[0].pid);
        assert!(
            exit[0].ns < starts[1].ns,
            "{name} started before its old VM exited: {ev:?}"
        );
        assert!(
            running[id].timings["teardown_ms"] >= 600,
            "{:?}",
            running[id].timings
        );
        exits.push(exit[0].ns);
    }
    // In parallel: the three 0.6 s teardowns ended within 0.3 s of each other, and the
    // whole relaunch took well under three teardowns back to back.
    let spread = exits.iter().max().unwrap() - exits.iter().min().unwrap();
    assert!(
        spread < 300_000_000,
        "teardowns ran one after another: spread {spread} ns"
    );
    assert!(
        all_back < Duration::from_millis(1800),
        "relaunch all took {all_back:?}"
    );
    // The stopping phase was reported for each slot at the new generation.
    for id in ids {
        assert!(c.seen.iter().any(|m| matches!(m, AgentMsg::Slot(r) if r.id == id && r.phase == "stopping" && r.generation == 2)));
    }
    c.close().await;
    let ev = events(f.dir.path());
    assert_eq!(
        ev.iter().filter(|e| e.kind == "exit").count(),
        6,
        "every VM stops when the input closes: {ev:?}"
    );
}

#[tokio::test]
async fn a_new_rect_is_posted_without_a_relaunch() {
    let f = fake(1, &[]);
    let mut c = Coord::start(&f, 1, cfg(&f.fcvm));
    c.next().await;
    c.send(scene());
    c.assign(&[("slot-1", 0, 1, rect(0.0))]);
    c.until(&["slot-1"], "running", 1).await;
    c.assign(&[("slot-1", 0, 1, rect(0.25))]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while f.scenes[0].lock().unwrap().len() < 2 {
        assert!(Instant::now() < deadline, "the new rect was not posted");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(f.scenes[0].lock().unwrap()[1]["rect"]["x"], 0.25);
    // A scene change is posted too.
    let mut s = Scene::new(1_000);
    s.time_of_day = 20.0;
    c.send(CoordMsg::Scene(SharedScene::new(&s, 32.0 / 9.0)));
    while f.scenes[0].lock().unwrap().len() < 3 {
        assert!(Instant::now() < deadline, "the new scene was not posted");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(f.scenes[0].lock().unwrap()[2]["time_of_day"], 20.0);
    let starts = events(f.dir.path())
        .iter()
        .filter(|e| e.kind == "start")
        .count();
    assert_eq!(starts, 1, "a rect or scene change must not relaunch");
    c.close().await;
}

#[tokio::test]
async fn a_dying_vm_is_relaunched_with_doubling_backoff() {
    let f = fake(
        1,
        &[
            ("FAKE_DIE_AFTER_SECS", "0.1"),
            ("FAKE_RESTORE_SECS", "0.05"),
        ],
    );
    let mut config = cfg(&f.fcvm);
    config.backoff_min = Duration::from_millis(200);
    config.backoff_max = Duration::from_millis(800);
    let mut c = Coord::start(&f, 1, config);
    c.next().await;
    c.send(scene());
    c.assign(&[("slot-1", 0, 1, None)]);
    // Wait for six deaths.
    let mut failed = 0;
    while failed < 6 {
        if let AgentMsg::Slot(r) = c.next().await {
            if r.phase == "failed" {
                failed += 1;
                assert!(
                    r.error.as_deref().unwrap_or("").contains("fcvm exited"),
                    "{r:?}"
                );
            }
        }
    }
    let ev = events(f.dir.path());
    let starts: Vec<u128> = ev
        .iter()
        .filter(|e| e.kind == "start")
        .map(|e| e.ns)
        .collect();
    let deaths: Vec<u128> = ev
        .iter()
        .filter(|e| e.kind == "died")
        .map(|e| e.ns)
        .collect();
    assert!(starts.len() >= 6 && deaths.len() >= 6, "{ev:?}");
    // The gap from each death to the next start: 200, 400, 800, 800, 800 ms (plus
    // scheduling), never a tight loop and never over the cap by much.
    let want = [200u128, 400, 800, 800, 800];
    for (i, w) in want.iter().enumerate() {
        let gap_ms = (starts[i + 1] - deaths[i]) / 1_000_000;
        assert!(gap_ms >= *w, "gap {i} was {gap_ms} ms, want at least {w}");
        assert!(gap_ms < *w + 400, "gap {i} was {gap_ms} ms, want about {w}");
    }
    c.close().await;
}

#[tokio::test]
async fn an_unassigned_slot_is_torn_down() {
    let f = fake(2, &[]);
    let mut c = Coord::start(&f, 2, cfg(&f.fcvm));
    c.next().await;
    c.send(scene());
    c.assign(&[("slot-1", 0, 1, None), ("slot-2", 1, 1, None)]);
    c.until(&["slot-1", "slot-2"], "running", 1).await;
    c.assign(&[("slot-2", 1, 1, None)]);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let ev = events(f.dir.path());
        if ev
            .iter()
            .any(|e| e.kind == "exit" && e.name == "fleet-slot-1")
        {
            assert!(!ev
                .iter()
                .any(|e| e.kind == "exit" && e.name == "fleet-slot-2"));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "slot-1 was not torn down: {ev:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    c.close().await;
}

#[tokio::test]
async fn a_gpu_this_host_lacks_is_reported_failed() {
    let f = fake(1, &[]);
    let mut c = Coord::start(&f, 1, cfg(&f.fcvm));
    c.next().await;
    c.assign(&[("slot-9", 7, 1, None)]);
    let r = c.until(&["slot-9"], "failed", 1).await;
    assert!(r["slot-9"]
        .error
        .as_deref()
        .unwrap()
        .contains("not on this host"));
    c.close().await;
}

/// The agent binary, killed outright: every fcvm it started gets SIGTERM from the kernel
/// (PR_SET_PDEATHSIG) and exits.
#[tokio::test]
async fn killing_the_agent_takes_its_vms_with_it() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let f = fake(2, &[]);
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_fcvm"))
        .args([
            "fleet",
            "agent",
            "--stdio",
            "--host",
            "gpu-host",
            "--image",
            "localhost/renderer",
        ])
        .args([
            "--base-port",
            &f.base.to_string(),
            "--gpus",
            "0000:01:00.0=T4,0000:02:00.0=T4",
        ])
        .arg("--fcvm")
        .arg(&f.fcvm)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    for m in [
        scene(),
        CoordMsg::Assign {
            slots: vec![
                AssignSlot {
                    id: "slot-1".into(),
                    sbdf: "0000:01:00.0".into(),
                    generation: 1,
                    rect: rect(0.0),
                },
                AssignSlot {
                    id: "slot-2".into(),
                    sbdf: "0000:02:00.0".into(),
                    generation: 1,
                    rect: rect(0.25),
                },
            ],
            region: None,
        },
    ] {
        let mut l = serde_json::to_vec(&m).unwrap();
        l.push(b'\n');
        stdin.write_all(&l).await.unwrap();
    }
    let mut running = 0;
    while running < 2 {
        let line = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .expect("agent output");
        if let Ok(AgentMsg::Slot(r)) = serde_json::from_str::<AgentMsg>(&line) {
            if r.phase == "running" {
                running += 1;
            }
        }
    }
    let pids: Vec<u32> = events(f.dir.path())
        .iter()
        .filter(|e| e.kind == "start")
        .map(|e| e.pid)
        .collect();
    assert_eq!(pids.len(), 2);
    child.start_kill().unwrap();
    child.wait().await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let ev = events(f.dir.path());
        let exited: Vec<u32> = ev
            .iter()
            .filter(|e| e.kind == "exit")
            .map(|e| e.pid)
            .collect();
        if pids.iter().all(|p| exited.contains(p)) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fcvm children outlived the agent: {ev:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The fake fcvm processes are gone, not just logged.
    for p in pids {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Path::new(&format!("/proc/{p}")).exists() {
            assert!(Instant::now() < deadline, "pid {p} still alive");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// A rect change that arrives between a slot's `running` report and its next pass is still
/// posted. The failpoint parks the slot task in that window while the test changes the
/// rect; before the fix the change was marked seen and never posted.
#[tokio::test]
async fn a_rect_change_right_after_running_is_posted() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let f = fake(1, &[]);
    let release = f.dir.path().join("release");
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_fcvm"))
        .args([
            "fleet",
            "agent",
            "--stdio",
            "--host",
            "gpu-host",
            "--image",
            "localhost/renderer",
        ])
        .args([
            "--base-port",
            &f.base.to_string(),
            "--gpus",
            "0000:01:00.0=T4",
        ])
        .arg("--fcvm")
        .arg(&f.fcvm)
        .env(
            "FCVM_FAILPOINT",
            format!(
                "fleet.agent.slot_running:block_until_file:{}",
                release.display()
            ),
        )
        .env("RUST_LOG", "fcvm::fleet::agent=debug")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut err = BufReader::new(child.stderr.take().unwrap()).lines();
    let send = |m: CoordMsg| {
        let mut l = serde_json::to_vec(&m).unwrap();
        l.push(b'\n');
        l
    };
    let assign = |x: f64| CoordMsg::Assign {
        slots: vec![AssignSlot {
            id: "slot-1".into(),
            sbdf: "0000:01:00.0".into(),
            generation: 1,
            rect: rect(x),
        }],
        region: None,
    };
    stdin.write_all(&send(scene())).await.unwrap();
    stdin.write_all(&send(assign(0.0))).await.unwrap();
    let mut wait_for = async |needle: &str| loop {
        let line = tokio::time::timeout(Duration::from_secs(30), err.next_line())
            .await
            .unwrap_or_else(|_| panic!("no {needle:?} on the agent's stderr"))
            .unwrap()
            .expect("agent stderr open");
        if line.contains(needle) {
            return;
        }
    };
    wait_for("FAILPOINT fleet.agent.slot_running reached").await;
    assert_eq!(f.scenes[0].lock().unwrap().len(), 1);
    stdin.write_all(&send(assign(0.25))).await.unwrap();
    wait_for("assignment applied").await;
    std::fs::write(&release, "").unwrap();
    wait_for("FAILPOINT fleet.agent.slot_running released").await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while f.scenes[0].lock().unwrap().len() < 2 {
        assert!(Instant::now() < deadline, "the rect change was dropped");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(f.scenes[0].lock().unwrap()[1]["rect"]["x"], 0.25);
    drop(stdin);
    tokio::time::timeout(Duration::from_secs(30), child.wait())
        .await
        .unwrap()
        .unwrap();
}

/// A spot interruption notice in instance metadata (IMDSv2) is reported as draining.
#[tokio::test]
async fn a_spot_notice_is_reported_as_draining() {
    use axum::routing::{get, put};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    let polls = Arc::new(AtomicU32::new(0));
    let p = polls.clone();
    let imds = axum::Router::new()
        .route("/latest/api/token", put(|| async { "token-1" }))
        .route(
            "/latest/meta-data/spot/instance-action",
            get(move |h: axum::http::HeaderMap| {
                let p = p.clone();
                async move {
                    assert_eq!(h["x-aws-ec2-metadata-token"], "token-1");
                    // Not scheduled for the first three polls.
                    if p.fetch_add(1, Ordering::SeqCst) < 3 {
                        (axum::http::StatusCode::NOT_FOUND, String::new())
                    } else {
                        (
                            axum::http::StatusCode::OK,
                            r#"{"action":"terminate","time":"2026-10-22T10:00:00Z"}"#.to_string(),
                        )
                    }
                }
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, imds).await.unwrap() });
    let f = fake(1, &[]);
    let mut c = Coord::start_with(&f, 1, cfg(&f.fcvm), Some(url));
    loop {
        if let AgentMsg::Draining { reason } = c.next().await {
            assert!(reason.contains("terminate"), "{reason}");
            break;
        }
    }
    assert!(polls.load(Ordering::SeqCst) >= 4);
    c.close().await;
}

/// With an aggregator port the agent runs `fcvm fleet aggregate` for its band, reports its
/// frame URL, and gives it the layout: the assigned region and every placed slot's renderer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_agent_runs_and_feeds_its_aggregator() {
    let f = fake(2, &[]);
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let (coord_side, agent_side) = tokio::io::duplex(1 << 20);
    let (agent_in, agent_out) = tokio::io::split(agent_side);
    let (coord_in, coord_out) = tokio::io::split(coord_side);
    let agent = Agent {
        host: "gpu-host".into(),
        gpus: gpus(2),
        capacity: 2,
        base_port: f.base,
        cfg: cfg(&f.fcvm),
        spot_notice_url: None,
        spot_poll: Duration::from_millis(50),
        aggregate: Some((
            port,
            vec![
                env!("CARGO_BIN_EXE_fcvm").to_string(),
                "fleet".into(),
                "aggregate".into(),
                "--codec".into(),
                "raw".into(),
            ],
        )),
    };
    let task = tokio::spawn(serve(agent, agent_in, agent_out));
    let (to_agent, _w) = spawn_writer::<CoordMsg, _>(coord_out, "agent");
    let mut from_agent = LineReader::new(coord_in, "agent");
    let region = Rect {
        x: 0.0,
        y: 0.0,
        w: 0.5,
        h: 0.5,
    };
    to_agent.send(scene()).unwrap();
    to_agent
        .send(CoordMsg::Assign {
            slots: vec![
                AssignSlot {
                    id: "slot-1".into(),
                    sbdf: "0000:01:00.0".into(),
                    generation: 1,
                    rect: Some(Rect {
                        x: 0.0,
                        y: 0.0,
                        w: 0.25,
                        h: 0.5,
                    }),
                },
                AssignSlot {
                    id: "slot-2".into(),
                    sbdf: "0000:02:00.0".into(),
                    generation: 1,
                    rect: None,
                },
            ],
            region: Some(region),
        })
        .unwrap();
    loop {
        let m = tokio::time::timeout(Duration::from_secs(30), from_agent.next::<AgentMsg>())
            .await
            .expect("an agent message")
            .unwrap()
            .unwrap();
        if let AgentMsg::Aggregate { frame_url } = m {
            assert_eq!(frame_url, format!("ws://gpu-host.example:{port}/frames"));
            break;
        }
    }
    let layout_url = format!("http://127.0.0.1:{port}/layout");
    let deadline = Instant::now() + Duration::from_secs(10);
    let layout = loop {
        let l: serde_json::Value = reqwest::get(&layout_url)
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if !l.is_null() {
            break l;
        }
        assert!(
            Instant::now() < deadline,
            "no layout reached the aggregator"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(layout["region"], serde_json::to_value(region).unwrap());
    assert_eq!(layout["epoch_unix_ms"], 1_000);
    // Only placed slots are in the region; slot-2 is on standby.
    assert_eq!(layout["tiles"].as_array().unwrap().len(), 1);
    assert_eq!(layout["tiles"][0]["id"], "slot-1");
    assert_eq!(
        layout["tiles"][0]["url"],
        format!("ws://127.0.0.1:{}/frames", f.base)
    );

    // A new region reaches it too.
    let wider = Rect {
        x: 0.0,
        y: 0.0,
        w: 0.5,
        h: 1.0,
    };
    to_agent
        .send(CoordMsg::Assign {
            slots: vec![AssignSlot {
                id: "slot-1".into(),
                sbdf: "0000:01:00.0".into(),
                generation: 1,
                rect: Some(wider),
            }],
            region: Some(wider),
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let l: serde_json::Value = reqwest::get(&layout_url)
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if l["region"] == serde_json::to_value(wider).unwrap() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the new region did not reach the aggregator"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Closing the agent stops the aggregator.
    drop(to_agent);
    tokio::time::timeout(Duration::from_secs(30), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while reqwest::get(&layout_url).await.is_ok() {
        assert!(
            Instant::now() < deadline,
            "the aggregator outlived its agent"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
