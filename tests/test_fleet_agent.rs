//! The fleet agent against a fake fcvm (tests/fixtures/fake-fcvm.sh) and fake renderers:
//! reconcile, parallel relaunch, teardown before relaunch, backoff, and VMs dying with
//! the agent.

mod fleet_support;

use fcvm::fleet::agent::slot::AgentCfg;
use fcvm::fleet::agent::{serve, Agent, AggregateCfg, AggregatePublic};
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
    budgets: Scenes,
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
    let (scenes, budgets) = start_renderers_with_budgets(dir.path(), base, listeners);
    Fake {
        dir,
        fcvm,
        base,
        scenes,
        budgets,
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
        ipv6_prefix: None,
        frame_zone: None,
        tls_publish_base: None,
        public_ipv4: None,
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
            store_ready_file: None,
            boot: Default::default(),
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
                    private_camera: None,
                    layout_generation: 0,
                    rect_from_frame: 0,
                    previous_rect: None,
                    standby: false,
                    warm: false,
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

async fn wait_len(
    list: &std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    n: usize,
    what: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while list.lock().unwrap().len() < n {
        assert!(Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A renderer that restarts inside its VM (the VM keeps running) answers `/health` with a
/// new `boot_id`, without the scene it was posted: the agent posts the scene and the budget
/// to it again, without a relaunch.
#[tokio::test]
async fn a_renderer_restarted_in_its_vm_gets_its_scene_again() {
    let f = fake(1, &[]);
    let boot = f.dir.path().join(format!("port-{}.boot", f.base));
    let set_boot = |id: &str| {
        let tmp = boot.with_extension("boot.tmp");
        std::fs::write(&tmp, id).unwrap();
        std::fs::rename(&tmp, &boot).unwrap();
    };
    set_boot("boot-1");
    let mut c = Coord::start(&f, 1, cfg(&f.fcvm));
    c.next().await;
    c.send(scene());
    c.send(CoordMsg::Budget(fleet_core::protocol::BudgetBody {
        budget: 500_000,
        t: 11.2,
    }));
    c.assign(&[("slot-1", 0, 1, rect(0.0))]);
    c.until(&["slot-1"], "running", 1).await;
    wait_len(&f.budgets[0], 1, "the budget was not posted").await;
    // Health is read every 200 ms; an unchanged boot id posts nothing again.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(f.scenes[0].lock().unwrap().len(), 1, "one scene post");
    assert_eq!(f.budgets[0].lock().unwrap().len(), 1, "one budget post");
    set_boot("boot-2");
    wait_len(
        &f.scenes[0],
        2,
        "the scene was not posted again after the renderer restarted",
    )
    .await;
    wait_len(
        &f.budgets[0],
        2,
        "the budget was not posted again after the renderer restarted",
    )
    .await;
    let scenes = f.scenes[0].lock().unwrap().clone();
    assert_eq!(scenes[1], scenes[0], "the same scene");
    let starts = events(f.dir.path())
        .iter()
        .filter(|e| e.kind == "start")
        .count();
    assert_eq!(starts, 1, "a renderer restart is not a relaunch");
    c.close().await;
}

/// The grid budget reaches a wall tile's renderer, again after every scene post (a new
/// scene clears the renderer's schedule), never a renderer drawing the whole image for a
/// claim; the agent forwards the renderer's `budget_fit` and `t`.
#[tokio::test]
async fn the_grid_budget_is_posted_to_wall_tiles_after_every_scene() {
    let f = fake(1, &[]);
    let mut c = Coord::start(&f, 1, cfg(&f.fcvm));
    c.next().await;
    c.send(scene());
    c.assign(&[("slot-1", 0, 1, rect(0.0))]);
    c.until(&["slot-1"], "running", 1).await;
    let grid = fleet_core::protocol::BudgetBody {
        budget: 500_000,
        t: 11.2,
    };
    c.send(CoordMsg::Budget(grid));
    wait_len(&f.budgets[0], 1, "the budget was not posted").await;
    assert_eq!(
        f.budgets[0].lock().unwrap()[0],
        serde_json::json!({"budget": 500_000, "t": 11.2})
    );
    loop {
        if let AgentMsg::Health(h) = c.next().await {
            assert_eq!(h.budget_fit, Some(1.0e6));
            assert_eq!(h.t, Some(1.5));
            assert_eq!(h.render_ms_max, Some(12.5));
            break;
        }
    }
    let mut s = Scene::new(1_000);
    s.time_of_day = 20.0;
    c.send(CoordMsg::Scene(SharedScene::new(&s, 32.0 / 9.0)));
    wait_len(
        &f.budgets[0],
        2,
        "the budget was not posted again after the scene",
    )
    .await;
    let posted = f.budgets[0].lock().unwrap().clone();
    assert_eq!(posted[1], posted[0]);

    // A claim: the renderer draws the whole image and chooses its own budget.
    c.send(CoordMsg::Assign {
        slots: vec![AssignSlot {
            id: "slot-1".into(),
            sbdf: "0000:01:00.0".into(),
            generation: 2,
            rect: None,
            private_camera: Some(vec![]),
            layout_generation: 0,
            rect_from_frame: 0,
            previous_rect: None,
            standby: false,
            warm: false,
        }],
        region: None,
    });
    c.until(&["slot-1"], "running", 2).await;
    let mut healths = 0;
    while healths < 3 {
        if let AgentMsg::Health(_) = c.next().await {
            healths += 1;
        }
    }
    assert_eq!(
        f.budgets[0].lock().unwrap().len(),
        2,
        "a claimed GPU got the grid budget"
    );
    c.assign(&[("slot-1", 0, 3, rect(0.0))]);
    c.until(&["slot-1"], "running", 3).await;
    wait_len(
        &f.budgets[0],
        3,
        "the budget was not posted when the claim ended",
    )
    .await;
    c.close().await;
}

/// A borrowed GPU (`private_camera` in its assignment) relaunches and draws the whole
/// image with the private camera; when the claim ends it relaunches again and draws its
/// tile of the shared scene.
#[tokio::test]
async fn a_private_session_draws_the_whole_image_with_its_own_camera() {
    let f = fake(1, &[]);
    let mut c = Coord::start(&f, 1, cfg(&f.fcvm));
    c.next().await;
    c.send(scene());
    c.assign(&[("slot-1", 0, 1, rect(0.25))]);
    c.until(&["slot-1"], "running", 1).await;
    let private = vec![fleet_core::scene::Keyframe {
        t: 3.0,
        pos: [1.0, 2.0, 3.0],
        look: [4.0, 2.0, 3.0],
        fov_deg: 50.0,
        intent: Some("claim-intent".into()),
    }];
    let slot =
        |generation: u64, rect: Option<Rect>, camera: Option<Vec<fleet_core::scene::Keyframe>>| {
            CoordMsg::Assign {
                slots: vec![AssignSlot {
                    id: "slot-1".into(),
                    sbdf: "0000:01:00.0".into(),
                    generation,
                    rect,
                    private_camera: camera,
                    layout_generation: 0,
                    rect_from_frame: 0,
                    previous_rect: None,
                    standby: false,
                    warm: false,
                }],
                region: None,
            }
        };
    c.send(slot(2, None, Some(private.clone())));
    c.until(&["slot-1"], "running", 2).await;
    let body = f.scenes[0].lock().unwrap().last().unwrap().clone();
    assert_eq!(
        body["rect"],
        serde_json::json!({"x": 0.0, "y": 0.0, "w": 1.0, "h": 1.0}),
        "a private session draws the whole image"
    );
    assert_eq!(body["camera"], serde_json::to_value(&private).unwrap());
    c.send(slot(3, rect(0.25), None));
    c.until(&["slot-1"], "running", 3).await;
    let body = f.scenes[0].lock().unwrap().last().unwrap().clone();
    assert_eq!(body["rect"]["x"], 0.25);
    assert_ne!(body["camera"], serde_json::to_value(&private).unwrap());
    let starts = events(f.dir.path())
        .iter()
        .filter(|e| e.kind == "start")
        .count();
    assert_eq!(starts, 3, "the claim and its end each restore a fresh VM");
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
                assert!(
                    r.error.as_deref().unwrap_or("").contains("fcvm exited"),
                    "{r:?}"
                );
                // Each failure says when the agent tries again.
                let want = [200u64, 400, 800, 800, 800, 800][failed];
                assert_eq!(r.retry_in_ms, Some(want), "{r:?}");
                failed += 1;
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
                    private_camera: None,
                    layout_generation: 0,
                    rect_from_frame: 0,
                    previous_rect: None,
                    standby: false,
                    warm: false,
                },
                AssignSlot {
                    id: "slot-2".into(),
                    sbdf: "0000:02:00.0".into(),
                    generation: 1,
                    rect: rect(0.25),
                    private_camera: None,
                    layout_generation: 0,
                    rect_from_frame: 0,
                    previous_rect: None,
                    standby: false,
                    warm: false,
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
            private_camera: None,
            layout_generation: 0,
            rect_from_frame: 0,
            previous_rect: None,
            standby: false,
            warm: false,
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
/// Serving browsers, the aggregator listens with TLS on its IPv4 port (with the renderers'
/// certificate file) and the agent reports the host's public IPv4 address with that port.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_agent_runs_and_feeds_its_aggregator() {
    let f = fake(2, &[]);
    let free_port = || {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    };
    let (port, tls_port) = (free_port(), free_port());
    let tls_dir = tempfile::tempdir().unwrap();
    let ck = rcgen::generate_simple_self_signed(vec!["agg.fleet.test".to_string()]).unwrap();
    std::fs::write(
        tls_dir.path().join("tls.json"),
        serde_json::json!({"cert": ck.cert.pem(), "key": ck.signing_key.serialize_pem()})
            .to_string(),
    )
    .unwrap();
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
        store_ready_file: None,
        boot: Default::default(),
        aggregate: Some(AggregateCfg {
            port,
            argv: vec![
                env!("CARGO_BIN_EXE_fcvm").to_string(),
                "fleet".into(),
                "aggregate".into(),
                "--codec".into(),
                "raw".into(),
            ],
            public: Some(AggregatePublic {
                tls_file: tls_dir.path().join("tls.json"),
                v4_port: tls_port,
                ipv4: Some("192.0.2.7".into()),
                ipv6: None,
            }),
        }),
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
                    private_camera: None,
                    layout_generation: 0,
                    rect_from_frame: 0,
                    previous_rect: None,
                    standby: false,
                    warm: false,
                },
                AssignSlot {
                    id: "slot-2".into(),
                    sbdf: "0000:02:00.0".into(),
                    generation: 1,
                    rect: None,
                    private_camera: None,
                    layout_generation: 0,
                    rect_from_frame: 0,
                    previous_rect: None,
                    standby: false,
                    warm: false,
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
        if let AgentMsg::Aggregate {
            frame_url,
            ipv6,
            ipv4,
            v4_port,
        } = m
        {
            assert_eq!(frame_url, format!("ws://gpu-host.example:{port}/frames"));
            assert_eq!(
                (ipv6, ipv4.as_deref(), v4_port),
                (None, Some("192.0.2.7"), Some(tls_port))
            );
            break;
        }
    }
    // Browsers: TLS on the IPv4 port, the stream and the health check only.
    let https = reqwest::Client::builder()
        .use_rustls_tls()
        .tls_built_in_root_certs(false)
        .add_root_certificate(reqwest::Certificate::from_pem(ck.cert.pem().as_bytes()).unwrap())
        .resolve(
            "agg.fleet.test",
            std::net::SocketAddr::from(([127, 0, 0, 1], tls_port)),
        )
        .no_proxy()
        .build()
        .unwrap();
    let r = https
        .get(format!("https://agg.fleet.test:{tls_port}/healthz"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = https
        .get(format!("https://agg.fleet.test:{tls_port}/layout"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404, "the layout is not served to browsers");
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
                private_camera: None,
                layout_generation: 0,
                rect_from_frame: 0,
                previous_rect: None,
                standby: false,
                warm: false,
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

fn standby_slot(generation: u64, standby: bool, rect: Option<Rect>) -> CoordMsg {
    CoordMsg::Assign {
        slots: vec![AssignSlot {
            id: "slot-1".into(),
            sbdf: "0000:01:00.0".into(),
            generation,
            rect,
            private_camera: None,
            layout_generation: 0,
            rect_from_frame: 0,
            previous_rect: None,
            standby,
            warm: false,
        }],
        region: None,
    }
}

/// A GPU in the standby pool runs no VM: the agent scrubs it, reports the scrub and then
/// `standby`; placed on the wall later, it starts a VM at once.
#[tokio::test]
async fn a_standby_gpu_is_scrubbed_and_runs_no_vm() {
    let f = fake(1, &[]);
    let mut config = cfg(&f.fcvm);
    config.scrub = true;
    let mut c = Coord::start(&f, 1, config);
    c.next().await;
    c.send(scene());
    c.send(standby_slot(1, true, None));
    let mut scrubbed = false;
    loop {
        match c.next().await {
            AgentMsg::Scrub { sbdf, ok, .. } => {
                assert_eq!(sbdf, "0000:01:00.0");
                assert!(ok);
                scrubbed = true;
            }
            AgentMsg::Slot(r) if r.phase == "standby" => {
                assert!(scrubbed, "standby is reported after the scrub");
                break;
            }
            AgentMsg::Slot(r) => panic!("a standby GPU reported {r:?}"),
            _ => {}
        }
    }
    let ev = events(f.dir.path());
    assert_eq!(ev.iter().filter(|e| e.kind == "start").count(), 0, "{ev:?}");
    assert_eq!(ev.iter().filter(|e| e.kind == "scrub").count(), 1, "{ev:?}");
    c.send(standby_slot(1, false, rect(0.0)));
    c.until(&["slot-1"], "running", 1).await;
    c.close().await;
}

/// A scrub that fails takes the GPU out of rotation: no VM, no standby report.
#[tokio::test]
async fn a_failed_scrub_quarantines_the_gpu() {
    let f = fake(1, &[("FAKE_SCRUB_EXIT", "1")]);
    let mut config = cfg(&f.fcvm);
    config.scrub = true;
    let mut c = Coord::start(&f, 1, config);
    c.next().await;
    c.send(scene());
    c.send(standby_slot(1, true, None));
    let q = loop {
        match c.next().await {
            AgentMsg::Quarantine { sbdf, error, .. } => break (sbdf, error),
            AgentMsg::Slot(r) => assert_ne!(r.phase, "standby", "{r:?}"),
            _ => {}
        }
    };
    assert_eq!(q.0, "0000:01:00.0");
    assert!(q.1.contains("left memory nonzero"), "{}", q.1);
    c.close().await;
}

/// Three relaunches in a row that fail before the first frame quarantine the GPU, and the
/// agent stops trying it.
#[tokio::test]
async fn three_failed_relaunches_quarantine_the_gpu() {
    let f = fake(1, &[("FAKE_FAIL_START", "restore")]);
    let mut config = cfg(&f.fcvm);
    config.backoff_min = Duration::from_millis(50);
    config.backoff_max = Duration::from_millis(50);
    let mut c = Coord::start(&f, 1, config);
    c.next().await;
    c.send(scene());
    c.assign(&[("slot-1", 0, 1, rect(0.0))]);
    let mut failed = Vec::new();
    loop {
        match c.next().await {
            AgentMsg::Slot(r) if r.phase == "failed" => failed.push(r.retry_in_ms),
            AgentMsg::Quarantine { error, .. } => {
                assert!(error.contains("the restore failed"), "{error}");
                break;
            }
            _ => {}
        }
    }
    assert_eq!(
        failed,
        vec![Some(50), Some(50), None],
        "the third failure quarantines"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    let starts = events(f.dir.path())
        .iter()
        .filter(|e| e.kind == "start")
        .count();
    assert_eq!(starts, 3, "no relaunch after the quarantine");
    c.close().await;
}

/// A failure whose error names the GPU quarantines it at once.
#[tokio::test]
async fn a_failure_naming_the_gpu_quarantines_it_at_once() {
    let f = fake(1, &[("FAKE_FAIL_START", "gpu")]);
    let mut c = Coord::start(&f, 1, cfg(&f.fcvm));
    c.next().await;
    c.send(scene());
    c.assign(&[("slot-1", 0, 1, rect(0.0))]);
    let error = loop {
        if let AgentMsg::Quarantine { error, .. } = c.next().await {
            break error;
        }
    };
    assert!(error.contains("0000:01:00.0"), "{error}");
    let starts = events(f.dir.path())
        .iter()
        .filter(|e| e.kind == "start")
        .count();
    assert_eq!(starts, 1);
    c.close().await;
}

/// The hello carries the modification time of the host's store-ready marker, so the
/// coordinator can show when the store was ready during a wake.
#[tokio::test]
async fn the_hello_reports_when_the_store_was_ready() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("store-ready");
    std::fs::write(&marker, "").unwrap();
    let want = std::fs::metadata(&marker)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let f = fake(1, &[]);
    let (agent_side, coord_side) = tokio::io::duplex(1 << 20);
    let (agent_in, agent_out) = tokio::io::split(agent_side);
    let (coord_in, _coord_out) = tokio::io::split(coord_side);
    let agent = Agent {
        host: "gpu-host".into(),
        gpus: gpus(1),
        capacity: 1,
        base_port: f.base,
        cfg: cfg(&f.fcvm),
        spot_notice_url: None,
        spot_poll: Duration::from_millis(50),
        aggregate: None,
        store_ready_file: Some(marker),
        boot: Default::default(),
    };
    let task = tokio::spawn(serve(agent, agent_in, agent_out));
    let mut from_agent = LineReader::new(coord_in, "agent");
    let hello = tokio::time::timeout(Duration::from_secs(10), from_agent.next::<AgentMsg>())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    match hello {
        AgentMsg::Hello { store_ready_at, .. } => assert_eq!(store_ready_at, Some(want)),
        other => panic!("first message {other:?}"),
    }
    task.abort();
}

type Seen = std::sync::Arc<std::sync::Mutex<Option<String>>>;

async fn accept_agent(
    listener: &tokio::net::TcpListener,
    seen: &Seen,
) -> tokio_tungstenite::WebSocketStream<tokio::net::TcpStream> {
    let seen = seen.clone();
    let (tcp, _) = listener.accept().await.unwrap();
    tokio_tungstenite::accept_hdr_async(
        tcp,
        move |req: &tokio_tungstenite::tungstenite::handshake::server::Request, resp| {
            *seen.lock().unwrap() = req
                .headers()
                .get("cf-access-client-id")
                .map(|v| v.to_str().unwrap().to_string());
            Ok(resp)
        },
    )
    .await
    .unwrap()
}

async fn next_agent_msg(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
) -> AgentMsg {
    use futures_util::StreamExt;
    loop {
        let m = tokio::time::timeout(Duration::from_secs(30), ws.next())
            .await
            .expect("a message")
            .unwrap()
            .unwrap();
        if let tokio_tungstenite::tungstenite::Message::Text(t) = m {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

/// `--dial`: the agent connects with its headers, and after the connection drops it
/// reconnects and starts with its hello and each slot's latest report, while the VM it
/// started keeps running.
#[tokio::test]
async fn a_dialing_agent_reconnects_and_replays_its_state_without_restarting_vms() {
    use fcvm::fleet::agent::dial::{run as dial, DialCfg};
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message;
    let f = fake(1, &[]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/agent", listener.local_addr().unwrap());
    let agent = Agent {
        host: "gpu-host".into(),
        gpus: gpus(1),
        capacity: 1,
        base_port: f.base,
        cfg: cfg(&f.fcvm),
        spot_notice_url: None,
        spot_poll: Duration::from_millis(50),
        aggregate: None,
        store_ready_file: None,
        boot: Default::default(),
    };
    let task = tokio::spawn(dial(
        agent,
        DialCfg {
            url,
            headers: vec![("CF-Access-Client-Id".into(), "id-for-tests".into())],
            backoff_min: Duration::from_millis(50),
            backoff_max: Duration::from_millis(200),
        },
    ));
    let seen_header = std::sync::Arc::new(std::sync::Mutex::new(None));
    let mut ws = accept_agent(&listener, &seen_header).await;
    assert_eq!(seen_header.lock().unwrap().as_deref(), Some("id-for-tests"));
    assert!(matches!(
        next_agent_msg(&mut ws).await,
        AgentMsg::Hello { .. }
    ));
    for m in [
        scene(),
        CoordMsg::Assign {
            slots: vec![AssignSlot {
                id: "slot-1".into(),
                sbdf: "0000:01:00.0".into(),
                generation: 1,
                rect: rect(0.0),
                private_camera: None,
                layout_generation: 0,
                rect_from_frame: 0,
                previous_rect: None,
                standby: false,
                warm: false,
            }],
            region: None,
        },
    ] {
        ws.send(Message::Text(serde_json::to_string(&m).unwrap().into()))
            .await
            .unwrap();
    }
    loop {
        if let AgentMsg::Slot(r) = next_agent_msg(&mut ws).await {
            if r.phase == "running" {
                break;
            }
        }
    }
    drop(ws);
    let mut ws = accept_agent(&listener, &seen_header).await;
    assert!(matches!(
        next_agent_msg(&mut ws).await,
        AgentMsg::Hello { .. }
    ));
    match next_agent_msg(&mut ws).await {
        AgentMsg::Slot(r) => assert_eq!(
            (r.id.as_str(), r.phase.as_str(), r.generation),
            ("slot-1", "running", 1)
        ),
        other => panic!("expected the slot's replayed report, got {other:?}"),
    }
    let starts = events(f.dir.path())
        .iter()
        .filter(|e| e.kind == "start")
        .count();
    assert_eq!(starts, 1, "the VM kept running across the reconnect");
    task.abort();
}

/// `PutSecretValue` without a `ClientRequestToken` is refused by Secrets Manager (400
/// InvalidRequestException: "You must provide a ClientRequestToken value"); the coordinator's
/// certificate renewal stored nothing that way. Each put carries a fresh token.
#[tokio::test]
async fn a_stored_secret_version_carries_a_fresh_client_request_token() {
    use fcvm::fleet::coord::secrets::Secrets;
    std::env::set_var("AWS_ACCESS_KEY_ID", "AKIDFORTESTS");
    std::env::set_var("AWS_SECRET_ACCESS_KEY", "secret-for-tests");
    let tokens = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let t2 = tokens.clone();
    let app = axum::Router::new().fallback(move |headers: axum::http::HeaderMap, body: String| {
        let t2 = t2.clone();
        async move {
            assert_eq!(headers["x-amz-target"], "secretsmanager.PutSecretValue");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            match v["ClientRequestToken"].as_str() {
                // The service's rule: 32 to 64 characters.
                Some(t) if (32..=64).contains(&t.len()) => {
                    t2.lock().unwrap().push(t.to_string());
                    (axum::http::StatusCode::OK, axum::Json(serde_json::json!({"VersionId": t})))
                }
                _ => (
                    axum::http::StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({"__type": "InvalidRequestException", "Message": "You must provide a ClientRequestToken value"})),
                ),
            }
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let secrets = Secrets::with_endpoint("us-east-2", &base).unwrap();
    secrets.put("fcvm-fleet/tls-wildcard", "{}").await.unwrap();
    secrets.put("fcvm-fleet/tls-wildcard", "{}").await.unwrap();
    let t = tokens.lock().unwrap().clone();
    assert_eq!(t.len(), 2);
    assert_ne!(t[0], t[1], "each version needs its own token");
}

/// The renderers' certificate in the renderer's TLS interface: `tls.json` with `cert` and
/// `key` from the secret (mode 0600), left alone while the secret is unchanged, replaced by
/// a new file after a renewal; each VM gets the directory read-only, `TLS_CERT_FILE` and
/// `TLS_PORT`.
#[tokio::test]
async fn the_agent_writes_the_certificate_for_its_renderers() {
    use fcvm::fleet::agent::tls;
    use fcvm::fleet::coord::secrets::Secrets;
    use std::os::unix::fs::PermissionsExt;
    std::env::set_var("AWS_ACCESS_KEY_ID", "AKIDFORTESTS");
    std::env::set_var("AWS_SECRET_ACCESS_KEY", "secret-for-tests");
    let doc = std::sync::Arc::new(std::sync::Mutex::new(
        serde_json::json!({"cert": "CHAIN-1", "key": "KEY-1", "domains": [], "not_after": "", "issued_at": "", "issuer": ""}),
    ));
    let d2 = doc.clone();
    let app = axum::Router::new().fallback(move |headers: axum::http::HeaderMap, body: String| {
        let d2 = d2.clone();
        async move {
            assert_eq!(headers["x-amz-target"], "secretsmanager.GetSecretValue");
            assert!(body.contains("fcvm-fleet/tls-wildcard"));
            axum::Json(serde_json::json!({"SecretString": d2.lock().unwrap().to_string()}))
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let secrets = Secrets::with_endpoint("us-east-2", &base).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let tls_dir = dir.path().join("tls");
    assert!(tls::refresh(&secrets, "fcvm-fleet/tls-wildcard", &tls_dir)
        .await
        .unwrap());
    let file = tls_dir.join("tls.json");
    let read = |p: &std::path::Path| -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap()
    };
    assert_eq!(
        read(&file),
        serde_json::json!({"cert": "CHAIN-1", "key": "KEY-1"})
    );
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    use std::os::unix::fs::MetadataExt;
    let inode = std::fs::metadata(&file).unwrap().ino();
    assert!(!tls::refresh(&secrets, "fcvm-fleet/tls-wildcard", &tls_dir)
        .await
        .unwrap());
    doc.lock().unwrap()["key"] = serde_json::json!("KEY-2");
    assert!(tls::refresh(&secrets, "fcvm-fleet/tls-wildcard", &tls_dir)
        .await
        .unwrap());
    assert_eq!(read(&file)["key"], "KEY-2");
    // A renewal is a new file renamed into place: the renderer notices the new inode.
    assert_ne!(std::fs::metadata(&file).unwrap().ino(), inode);
    assert_eq!(
        tls::vm_args(&tls_dir),
        vec![
            "--map".to_string(),
            format!("{}:/run/fleet-tls:ro", tls_dir.display()),
            "--env".to_string(),
            "TLS_CERT_FILE=/run/fleet-tls/tls.json".to_string(),
            "--env".to_string(),
            "TLS_PORT=443".to_string(),
        ]
    );
}

/// A moved tile's renderer is told the new rectangle, the layout generation, the frame it
/// takes effect at and the rectangle before, so it switches at that frame.
#[tokio::test]
async fn the_renderer_gets_the_layout_generation_with_its_rect() {
    let f = fake(1, &[]);
    let mut c = Coord::start(&f, 1, cfg(&f.fcvm));
    c.next().await;
    c.send(scene());
    let before = rect(0.0).unwrap();
    c.send(CoordMsg::Assign {
        slots: vec![AssignSlot {
            id: "slot-1".into(),
            sbdf: "0000:01:00.0".into(),
            generation: 1,
            rect: rect(0.25),
            private_camera: None,
            layout_generation: 3,
            rect_from_frame: 1234,
            previous_rect: Some(before),
            standby: false,
            warm: false,
        }],
        region: None,
    });
    c.until(&["slot-1"], "running", 1).await;
    let body = f.scenes[0].lock().unwrap().last().unwrap().clone();
    assert_eq!(body["rect"]["x"], 0.25);
    assert_eq!(body["layout_generation"], 3);
    assert_eq!(body["rect_from_frame"], 1234);
    assert_eq!(body["previous_rect"]["x"], 0.0);
    c.close().await;
}

/// With a host prefix each slot runs routed at its own address (`<prefix>::<GPU index + 2>`),
/// reports it for its DNS name, and with a frame zone its frame URL is that name over TLS.
#[tokio::test]
async fn each_slot_runs_routed_at_its_own_address() {
    let f = fake(2, &[]);
    let mut cfg = cfg(&f.fcvm);
    cfg.ipv6_prefix = Some(fcvm::fleet::agent::addr::Prefix::parse("2001:db8:1:2:3::/80").unwrap());
    cfg.frame_zone = Some("fleet.example".into());
    let mut c = Coord::start(&f, 2, cfg);
    c.next().await;
    c.send(scene());
    c.assign(&[("slot-1", 0, 1, rect(0.0)), ("slot-4", 1, 1, rect(0.25))]);
    let got = c.until(&["slot-1", "slot-4"], "running", 1).await;
    assert_eq!(got["slot-1"].ipv6.as_deref(), Some("2001:db8:1:2:3::2"));
    assert_eq!(got["slot-4"].ipv6.as_deref(), Some("2001:db8:1:2:3::3"));
    assert_eq!(
        got["slot-1"].frame_url.as_deref(),
        Some("wss://slot-1.fleet.example/frames")
    );
    let args = std::fs::read_to_string(f.dir.path().join("args-fleet-slot-4")).unwrap();
    assert!(
        args.contains(
            "--network\nrouted\n--ipv6-prefix\n2001:db8:1:2:3::/80\n--ipv6-address\n2001:db8:1:2:3::3\n"
        ),
        "{args}"
    );
    c.close().await;
}

/// Without routed VMs, the renderer's TLS port is published per slot (base + GPU index) and
/// the zone frame URL names it.
#[tokio::test]
async fn a_slot_publishes_its_tls_port_when_not_routed() {
    let f = fake(2, &[]);
    let mut cfg = cfg(&f.fcvm);
    cfg.frame_zone = Some("fleet.example".into());
    cfg.tls_publish_base = Some(40100);
    let mut c = Coord::start(&f, 2, cfg);
    c.next().await;
    c.send(scene());
    c.assign(&[("slot-2", 1, 1, rect(0.0))]);
    let got = c.until(&["slot-2"], "running", 1).await;
    assert_eq!(
        got["slot-2"].frame_url.as_deref(),
        Some("wss://slot-2.fleet.example:40101/frames")
    );
    let args = std::fs::read_to_string(f.dir.path().join("args-fleet-slot-2")).unwrap();
    assert!(args.contains("--publish\n127.0.0.1:40101:443\n"), "{args}");
    c.close().await;
}

/// A routed slot's IPv4 path: the host's one public IPv4 address with a port per slot. The
/// slot forwards its TLS port on the host (base + GPU index, into the routed VM) and reports
/// the host's address and its port; its frame URL stays its own IPv6 name on 443.
#[tokio::test]
async fn a_routed_slot_reports_the_hosts_ipv4_and_its_port() {
    let f = fake(2, &[]);
    let mut cfg = cfg(&f.fcvm);
    cfg.ipv6_prefix = Some(fcvm::fleet::agent::addr::Prefix::parse("2001:db8:1:2:3::/80").unwrap());
    cfg.frame_zone = Some("fleet.example".into());
    cfg.tls_publish_base = Some(20200);
    cfg.publish_ip = "0.0.0.0".into();
    cfg.public_ipv4 = Some("192.0.2.44".into());
    let mut c = Coord::start(&f, 2, cfg);
    c.next().await;
    c.send(scene());
    c.assign(&[("slot-1", 0, 1, rect(0.0)), ("slot-2", 1, 1, rect(0.25))]);
    let got = c.until(&["slot-1", "slot-2"], "running", 1).await;
    for (id, port) in [("slot-1", 20200), ("slot-2", 20201)] {
        assert_eq!(got[id].ipv4.as_deref(), Some("192.0.2.44"), "{id}");
        assert_eq!(got[id].v4_port, Some(port), "{id}");
        assert_eq!(
            got[id].frame_url.as_deref(),
            Some(format!("wss://{id}.fleet.example/frames").as_str())
        );
    }
    let args = std::fs::read_to_string(f.dir.path().join("args-fleet-slot-2")).unwrap();
    assert!(args.contains("--publish\n0.0.0.0:20201:443\n"), "{args}");
    assert!(args.contains("--network\nrouted\n"), "{args}");
    c.close().await;
}

/// A warming slot runs its VM and draws the whole image (it is not on the wall yet); placed
/// at the same generation, it takes its rectangle without a relaunch.
#[tokio::test]
async fn a_warming_slot_draws_the_whole_image_then_takes_its_place() {
    let f = fake(1, &[]);
    let mut c = Coord::start(&f, 1, cfg(&f.fcvm));
    c.next().await;
    c.send(scene());
    let slot = |rect: Option<Rect>, warm: bool| CoordMsg::Assign {
        slots: vec![AssignSlot {
            id: "slot-1".into(),
            sbdf: "0000:01:00.0".into(),
            generation: 1,
            rect,
            private_camera: None,
            layout_generation: 2,
            rect_from_frame: 900,
            previous_rect: None,
            standby: false,
            warm,
        }],
        region: None,
    };
    c.send(slot(None, true));
    c.until(&["slot-1"], "running", 1).await;
    let body = f.scenes[0].lock().unwrap().last().unwrap().clone();
    assert_eq!(
        body["rect"],
        serde_json::json!({"x": 0.0, "y": 0.0, "w": 1.0, "h": 1.0}),
        "a warming slot draws the whole image"
    );
    c.send(slot(rect(0.25), false));
    let deadline = Instant::now() + Duration::from_secs(10);
    while f.scenes[0].lock().unwrap().last().unwrap()["rect"]["x"] != 0.25 {
        assert!(Instant::now() < deadline, "the placed rect was not posted");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let starts = events(f.dir.path())
        .iter()
        .filter(|e| e.kind == "start")
        .count();
    assert_eq!(starts, 1, "joining the wall must not relaunch");
    c.close().await;
}

/// The hello's boot times: the kernel's start from `/proc/uptime`, the agent's start, and
/// every `*_at` number in the store unit's ready file; anything else in that file is left
/// out, and a file that is not a JSON object adds nothing.
#[test]
fn boot_times_come_from_uptime_the_agent_and_the_store_ready_file() {
    use fcvm::fleet::agent::boot_times;
    let b = boot_times(
        Some("86.25 1201.50\n"),
        1_000_000,
        999_000,
        Some(
            r#"{"store_started_at": 920000, "store_mounted_at": 921000, "store_source": "s3", "bytes": 5}"#,
        ),
    );
    assert_eq!(
        b,
        [
            ("kernel_started_at".to_string(), 1_000_000 - 86_250),
            ("agent_started_at".to_string(), 999_000),
            ("store_started_at".to_string(), 920_000),
            ("store_mounted_at".to_string(), 921_000),
        ]
        .into()
    );
    // The store unit of an older image wrote nothing (or an empty marker).
    assert_eq!(
        boot_times(None, 1_000_000, 999_000, Some("")),
        [("agent_started_at".to_string(), 999_000)].into()
    );
}
