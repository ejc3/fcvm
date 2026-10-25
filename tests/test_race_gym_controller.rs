//! The race gym's controller against toy actors (no VM, no KVM): shell scripts that speak
//! the failpoint park protocol around a planted check-then-act race. The pair search must
//! find the race, and the replay of the failing schedule must fail the same way, every run.

#[path = "race_gym/mod.rs"]
mod race_gym;

use anyhow::Result;
use race_gym::{Event, Failure, Limits, Plan, RunCtx, Scenario, Schedule};
use std::path::PathBuf;
use std::time::Duration;

/// Each actor: create the shared file if it is missing, read it, remove it. An actor that
/// reads after the other removed it fails. Lock-step runs pass; holding one actor before its
/// read while the other removes the file does not.
const TOY: &str = r#"
set -u
dir="$PARK/$FCVM_RACE_ACTOR-$$"; mkdir -p "$dir"; seq=0; declare -A hits
park() {
  hits[$1]=$(( ${hits[$1]:-0} + 1 ))
  n=$(printf %06d $seq); seq=$((seq + 1))
  printf 'point=%s\nhit=%s\npid=%s\n' "$1" "${hits[$1]}" $$ > "$dir/.$n.tmp"
  mv "$dir/.$n.tmp" "$dir/$n.reached"
  while [ ! -e "$dir/$n.go" ]; do sleep 0.005; done
}
park toy.start
[ -e "$SHARED/f" ] || { park toy.pre_create; echo data > "$SHARED/f"; }
park toy.pre_read
cat "$SHARED/f" > /dev/null || { echo "ERROR: shared file vanished before the read" >&2; exit 3; }
park toy.pre_remove
rm -f "$SHARED/f"
"#;

struct Toy {
    root: PathBuf,
}

impl Scenario for Toy {
    fn name(&self) -> &str {
        "toy"
    }
    fn actors(&self) -> usize {
        2
    }
    fn params(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    fn command(&self, run: &RunCtx, _idx: usize) -> Result<tokio::process::Command> {
        let shared = self.root.join(&run.tag).join("shared");
        std::fs::create_dir_all(&shared)?;
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c")
            .arg(TOY)
            .env("PARK", &run.park_dir)
            .env("SHARED", shared);
        Ok(cmd)
    }
    fn end_oracles<'a>(
        &'a self,
        run: &'a RunCtx,
        exits: &'a [i32],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<Failure>> + Send + 'a>> {
        Box::pin(async move {
            exits
                .iter()
                .enumerate()
                .filter(|(_, c)| **c != 0)
                .map(|(i, c)| {
                    Failure::new(
                        "outcome",
                        Some(i),
                        format!("exit {c}: {}", race_gym::error_lines(&run.actor_log(i), 1)),
                    )
                })
                .collect()
        })
    }
}

fn ctx(root: &std::path::Path, tag: &str) -> RunCtx {
    RunCtx {
        tag: tag.to_string(),
        park_dir: root.join(tag).join("park"),
        log_dir: root.join(tag),
        slot: 0,
    }
}

#[tokio::test]
async fn race_gym_pair_search_finds_the_toy_race_and_replay_reproduces_it() -> Result<()> {
    let root = scripts_root("toy");
    let toy = Toy { root: root.clone() };
    let limits = Limits {
        step_deadline: Duration::from_secs(20),
    };

    let discovery = race_gym::run(
        &toy,
        &ctx(&root, "discovery"),
        &limits,
        Plan::Schedule(&Schedule::Lockstep),
    )
    .await?;
    assert!(
        discovery.failed.is_empty(),
        "lock-step must pass: {:?}",
        discovery.failed
    );
    assert_eq!(
        discovery.traces[0],
        [
            "toy.start#1",
            "toy.pre_create#1",
            "toy.pre_read#1",
            "toy.pre_remove#1"
        ],
        "discovery trace"
    );

    let schedules = race_gym::pair_schedules(&discovery.traces[0], &discovery.traces[1]);
    assert_eq!(schedules.len(), 2 * 4 * 4);
    let mut failing = Vec::new();
    for (n, schedule) in schedules.iter().enumerate() {
        let result = race_gym::run(
            &toy,
            &ctx(&root, &format!("pair{n}")),
            &limits,
            Plan::Schedule(schedule),
        )
        .await?;
        assert!(
            result.failed.iter().all(|f| f.oracle == "outcome"),
            "schedule {schedule:?}: {:?}",
            result.failed
        );
        if !result.failed.is_empty() {
            failing.push((schedule.clone(), result));
        }
    }
    // Every failure is the planted one: a read after the other actor's remove.
    assert!(
        !failing.is_empty(),
        "the pair search missed the planted race"
    );
    for (_, result) in &failing {
        assert!(
            result.failed[0].detail.contains("vanished"),
            "{:?}",
            result.failed
        );
    }
    // The minimal witness: actor 0 held before its remove, actor 1 before its read, 0 first.
    let witness = Schedule::Pair {
        a: 0,
        i: "toy.pre_remove#1".into(),
        b: 1,
        j: "toy.pre_read#1".into(),
        first: 0,
    };
    let (_, found) = failing
        .iter()
        .find(|(s, _)| *s == witness)
        .expect("the minimal witness is among the failing schedules");
    assert_eq!(found.failed[0].actor, Some(1));

    // Replaying the witness's steps fails the same way every time and diverges nowhere.
    for rep in 0..5 {
        let replayed = race_gym::run(
            &toy,
            &ctx(&root, &format!("replay{rep}")),
            &limits,
            Plan::Replay(&found.steps),
        )
        .await?;
        assert_eq!(
            replayed
                .failed
                .iter()
                .map(Failure::signature)
                .collect::<Vec<_>>(),
            found
                .failed
                .iter()
                .map(Failure::signature)
                .collect::<Vec<_>>(),
            "replay {rep}"
        );
        assert_eq!(replayed.steps, found.steps, "replay {rep} steps");
    }

    // A replay whose recorded event no longer happens is a divergence, not a pass.
    let mut tampered = found.steps.clone();
    let last = tampered.len() - 1;
    tampered[last].event = match &found.steps[last].event {
        Event::Exited(0) => Event::Exited(7),
        _ => Event::Exited(0),
    };
    let diverged = race_gym::run(
        &toy,
        &ctx(&root, "tampered"),
        &limits,
        Plan::Replay(&tampered),
    )
    .await?;
    assert!(
        diverged
            .failed
            .iter()
            .any(|f| f.oracle == "control" && f.detail.contains("REPLAY DIVERGED")),
        "{:?}",
        diverged.failed
    );

    // Two seeded walks with the same seed take the same steps.
    let walk = Schedule::Walk {
        seed: 7,
        depth: 2,
        steps_hint: 8,
    };
    let w1 = race_gym::run(&toy, &ctx(&root, "walk1"), &limits, Plan::Schedule(&walk)).await?;
    let w2 = race_gym::run(&toy, &ctx(&root, "walk2"), &limits, Plan::Schedule(&walk)).await?;
    assert_eq!(w1.steps, w2.steps, "a seeded walk must repeat");

    std::fs::remove_dir_all(&root)?;
    Ok(())
}

/// A step that produces no park, note or exit fails the schedule instead of passing it.
#[tokio::test]
async fn race_gym_a_silent_actor_fails_as_an_unmodeled_wait() -> Result<()> {
    struct Silent;
    impl Scenario for Silent {
        fn name(&self) -> &str {
            "silent"
        }
        fn actors(&self) -> usize {
            1
        }
        fn params(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn command(&self, _run: &RunCtx, _idx: usize) -> Result<tokio::process::Command> {
            let mut cmd = tokio::process::Command::new("sleep");
            cmd.arg("30");
            Ok(cmd)
        }
        fn end_oracles<'a>(
            &'a self,
            _run: &'a RunCtx,
            _exits: &'a [i32],
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<Failure>> + Send + 'a>>
        {
            Box::pin(async { Vec::new() })
        }
    }
    let root = scripts_root("silent");
    let limits = Limits {
        step_deadline: Duration::from_millis(500),
    };
    let result = race_gym::run(
        &Silent,
        &ctx(&root, "s"),
        &limits,
        Plan::Schedule(&Schedule::Lockstep),
    )
    .await?;
    assert!(
        result
            .failed
            .iter()
            .any(|f| f.detail.contains("UNMODELED WAIT")),
        "{:?}",
        result.failed
    );
    std::fs::remove_dir_all(&root)?;
    Ok(())
}

/// An actor with two parked hits at once (fcvm's health monitor and its main task): when one
/// is released and its task produces nothing more, the step's event is the other hit, which
/// was already parked. Seen on the GPU host as an unmodeled wait in 1 of 660 schedules.
#[tokio::test]
async fn race_gym_a_second_parked_hit_of_the_released_actor_is_its_event() -> Result<()> {
    // main parks at m0. Once released, two hits appear in one scan: a task that parks at
    // bg.b and then never parks again, and main parked at m1. They are staged in one
    // directory renamed into place, so the controller cannot see one without the other.
    const TWO: &str = r#"
set -u
dir="$PARK/$FCVM_RACE_ACTOR-$$"; mkdir -p "$dir"
printf 'point=main.m0\nhit=1\npid=%s\n' $$ > "$dir/.0.tmp"; mv "$dir/.0.tmp" "$dir/000000.reached"
while [ ! -e "$dir/000000.go" ]; do sleep 0.005; done
two="$PARK/$FCVM_RACE_ACTOR-${$}9"; stage="$PARK/.stage-$$"; mkdir -p "$stage"
printf 'point=bg.b\nhit=1\npid=%s\n' $$ > "$stage/000000.reached"
printf 'point=main.m1\nhit=1\npid=%s\n' $$ > "$stage/000001.reached"
( while [ ! -e "$two/000000.go" ]; do sleep 0.005; done; exec sleep 1000 ) & bg=$!
mv "$stage" "$two"
while [ ! -e "$two/000001.go" ]; do sleep 0.005; done
kill $bg
"#;
    struct Two;
    impl Scenario for Two {
        fn name(&self) -> &str {
            "two"
        }
        fn actors(&self) -> usize {
            1
        }
        fn params(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn command(&self, run: &RunCtx, _idx: usize) -> Result<tokio::process::Command> {
            let mut cmd = tokio::process::Command::new("bash");
            cmd.arg("-c").arg(TWO).env("PARK", &run.park_dir);
            Ok(cmd)
        }
        fn end_oracles<'a>(
            &'a self,
            _run: &'a RunCtx,
            _exits: &'a [i32],
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<Failure>> + Send + 'a>>
        {
            Box::pin(async { Vec::new() })
        }
    }
    let root = scripts_root("two");
    let limits = Limits {
        step_deadline: Duration::from_secs(5),
    };
    let result = race_gym::run(
        &Two,
        &ctx(&root, "t"),
        &limits,
        Plan::Schedule(&Schedule::Lockstep),
    )
    .await?;
    assert!(result.failed.is_empty(), "{:?}", result.failed);
    let releases: Vec<&str> = result.steps.iter().map(|s| s.release.as_str()).collect();
    assert_eq!(releases, ["main.m0#1", "bg.b#1@1", "main.m1#1@1"]);
    assert_eq!(result.steps[1].event, Event::Parked("main.m1#1@1".into()));
    std::fs::remove_dir_all(&root)?;
    Ok(())
}

/// Shell actors, one per script, for the controller tests below. `park NAME SEQ` and
/// `note KIND DETAIL SEQ` speak the failpoint protocol under `$PARK/$FCVM_RACE_ACTOR-$$`.
struct Scripts(Vec<&'static str>);

const PROTOCOL: &str = r#"
set -u
dir="$PARK/$FCVM_RACE_ACTOR-$$"; mkdir -p "$dir"
park() { n=$(printf %06d $2); printf 'point=%s\nhit=1\npid=%s\n' "$1" $$ > "$dir/.$n.tmp"; mv "$dir/.$n.tmp" "$dir/$n.reached"; while [ ! -e "$dir/$n.go" ]; do sleep 0.005; done; }
note() { n=$(printf %06d $3); printf 'note=%s\ndetail=%s\npid=%s\n' "$1" "$2" $$ > "$dir/.$n.tmp"; mv "$dir/.$n.tmp" "$dir/$n.note"; }
"#;

impl Scenario for Scripts {
    fn name(&self) -> &str {
        "scripts"
    }
    fn actors(&self) -> usize {
        self.0.len()
    }
    fn params(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    fn command(&self, run: &RunCtx, idx: usize) -> Result<tokio::process::Command> {
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c")
            .arg(format!("{PROTOCOL}{}", self.0[idx]))
            .env("PARK", &run.park_dir)
            .env("SHARED", run.log_dir.join("shared"));
        Ok(cmd)
    }
    fn end_oracles<'a>(
        &'a self,
        _run: &'a RunCtx,
        exits: &'a [i32],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<Failure>> + Send + 'a>> {
        Box::pin(async move {
            exits
                .iter()
                .enumerate()
                .filter(|(_, c)| **c != 0)
                .map(|(i, c)| Failure::new("outcome", Some(i), format!("exit {c}")))
                .collect()
        })
    }
}

/// A test's own directory, `race-gym-<name>-<pid>` under the temp dir, emptied first. A test
/// removes it when it passes and leaves it for diagnosis when it fails, so a later run with
/// the same pid would otherwise find the failed run's files.
fn scripts_root(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("race-gym-{name}-{}", std::process::id()));
    if let Err(error) = std::fs::remove_dir_all(&root) {
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::NotFound,
            "clearing {}: {error}",
            root.display()
        );
    }
    root
}

/// Actor 0 waits on a lock actor 1 holds and exits a moment after actor 1's last release,
/// with no step of its own. The replay waits for that exit as the recording did.
#[tokio::test]
async fn race_gym_a_replay_waits_for_exits_after_its_last_step() -> Result<()> {
    let root = scripts_root("late-exit");
    let actors = Scripts(vec![
        r#"park a.first 0; note lock.wait shared/held.lock 1; while [ ! -e "$SHARED/free" ]; do sleep 0.01; done; sleep 0.3"#,
        r#"park b.first 0; mkdir -p "$SHARED"; touch "$SHARED/free""#,
    ]);
    let limits = Limits {
        step_deadline: Duration::from_secs(5),
    };
    let recorded = race_gym::run(
        &actors,
        &ctx(&root, "rec"),
        &limits,
        Plan::Schedule(&Schedule::Lockstep),
    )
    .await?;
    assert!(recorded.failed.is_empty(), "{:?}", recorded.failed);
    for rep in 0..3 {
        let replayed = race_gym::run(
            &actors,
            &ctx(&root, &format!("rep{rep}")),
            &limits,
            Plan::Replay(&recorded.steps),
        )
        .await?;
        assert!(
            replayed.failed.is_empty(),
            "replay {rep}: {:?}",
            replayed.failed
        );
    }
    std::fs::remove_dir_all(&root)?;
    Ok(())
}

/// The first release waits until every actor has parked at its first point, so a slow
/// starter does not change the order of a lock-step run.
#[tokio::test]
async fn race_gym_lockstep_starts_when_every_actor_has_parked() -> Result<()> {
    let root = scripts_root("barrier");
    let actors = Scripts(vec![
        r#"park a.one 0; park a.two 1"#,
        r#"sleep 0.4; park b.one 0; park b.two 1"#,
    ]);
    let limits = Limits {
        step_deadline: Duration::from_secs(5),
    };
    let result = race_gym::run(
        &actors,
        &ctx(&root, "b"),
        &limits,
        Plan::Schedule(&Schedule::Lockstep),
    )
    .await?;
    assert!(result.failed.is_empty(), "{:?}", result.failed);
    let order: Vec<(usize, &str)> = result
        .steps
        .iter()
        .map(|s| (s.actor, s.release.as_str()))
        .collect();
    assert_eq!(
        order,
        [
            (0, "a.one#1"),
            (1, "b.one#1"),
            (0, "a.two#1"),
            (1, "b.two#1")
        ]
    );
    std::fs::remove_dir_all(&root)?;
    Ok(())
}

/// A park that reached its cap and ran on unreleased fails the schedule.
#[tokio::test]
async fn race_gym_a_park_timeout_fails_the_schedule() -> Result<()> {
    let root = scripts_root("park-timeout");
    let actors = Scripts(vec![r#"park a.one 0; note park.timeout a.two 1"#]);
    let limits = Limits {
        step_deadline: Duration::from_secs(5),
    };
    let result = race_gym::run(
        &actors,
        &ctx(&root, "t"),
        &limits,
        Plan::Schedule(&Schedule::Lockstep),
    )
    .await?;
    assert!(
        result
            .failed
            .iter()
            .any(|f| f.oracle == "control" && f.detail.contains("PARK TIMED OUT")),
        "{:?}",
        result.failed
    );
    std::fs::remove_dir_all(&root)?;
    Ok(())
}

/// Only a lock one run owns can make a pair infeasible; a shared one is waited out.
#[test]
fn race_gym_only_run_owned_locks_make_a_pair_infeasible() {
    for owned in [
        "snapshots/#.lock",
        "vm-disks/vm-#",
        "vm-#/snapshot.lock",
        "state/vm-#.json.lock",
    ] {
        assert!(race_gym::run_owned_lock(owned), "{owned}");
    }
    for shared in [
        "state/names.lock",
        "state/loopback-ip.lock",
        "image-cache/#.lock",
        "rootfs/.rootfs-creation.lock",
    ] {
        assert!(!race_gym::run_owned_lock(shared), "{shared}");
    }
}

/// Every process the gym starts is armed to die with the thread that started it, so a gym
/// that is SIGKILLed mid-run leaves no fcvm (and no VM) behind. `spawn_recorded` arms it for
/// every caller: the actors and a scenario's own commands, such as cold-run's fresh restore.
#[test]
fn race_gym_a_spawned_process_dies_with_its_spawner() {
    let pid = std::thread::spawn(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut cmd = tokio::process::Command::new("sleep");
            cmd.arg("1000");
            let child = race_gym::spawn_recorded(&mut cmd).unwrap();
            child.id().unwrap()
        })
    })
    .join()
    .unwrap();
    // The spawning thread has exited. The kernel's SIGKILL leaves a zombie nobody reaps,
    // so the pid cannot be reused while this polls.
    let state = || {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| s.rsplit_once(") ").map(|(_, r)| r.chars().next().unwrap()))
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while matches!(state(), Some(s) if s != 'Z') && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let last = state();
    if matches!(last, Some(s) if s != 'Z') {
        // SAFETY: plain kill of the sleep this test started.
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        panic!("sleep {pid} outlived the thread that spawned it (state {last:?})");
    }
}

/// A discovery run that fails is part of the gym's verdict, not a printed line, and traces
/// cut short by a failure are not used for the search.
#[tokio::test]
async fn race_gym_a_failed_discovery_fails_the_gym() -> Result<()> {
    let root = scripts_root("discovery");
    let actors = Scripts(vec![
        r#"park a.one 0; park a.two 1; exit 3"#,
        r#"park b.one 0; park b.two 1"#,
    ]);
    let limits = Limits {
        step_deadline: Duration::from_secs(5),
    };
    let discovery = race_gym::run(
        &actors,
        &ctx(&root, "d"),
        &limits,
        Plan::Schedule(&Schedule::Lockstep),
    )
    .await?;
    assert!(
        discovery.traces.iter().all(|t| !t.is_empty()) && !discovery.failed.is_empty(),
        "the fixture must fail with every actor traced: traces {:?}, failed {:?}",
        discovery.traces,
        discovery.failed
    );
    let error = race_gym::discovery_findings(&discovery)
        .expect_err("a discovery run cut short by a failure was accepted");
    assert!(format!("{error:#}").contains("exit 3"), "{error:#}");

    // An end-of-run oracle leaves the traces whole: the search goes on, and the failure is
    // returned to be reported, not dropped.
    let mut whole = discovery;
    whole.failed = vec![Failure::new("leak", None, "vm-disks/vm-1 left behind")];
    assert_eq!(race_gym::discovery_findings(&whole)?, whole.failed);
    std::fs::remove_dir_all(&root)?;
    Ok(())
}

/// A test starts from an empty directory even when a failed run with the same pid left its
/// files behind (the toy skips its create step when the shared file already exists).
#[test]
fn race_gym_a_test_root_starts_empty() {
    let stale = std::env::temp_dir().join(format!("race-gym-stale-{}", std::process::id()));
    std::fs::create_dir_all(stale.join("shared")).unwrap();
    std::fs::write(stale.join("shared/f"), "left by a failed run").unwrap();
    let root = scripts_root("stale");
    assert_eq!(root, stale);
    let left = std::fs::read_dir(&root).map(|d| d.count()).unwrap_or(0);
    let _ = std::fs::remove_dir_all(&stale);
    assert_eq!(
        left,
        0,
        "{} still holds a previous run's files",
        root.display()
    );
}

/// `make race-gym EMIT=` writes the recorded failure into a `///` comment. A failure detail
/// that spans lines (a control failure's error chain) must stay inside the comment, or the
/// emitted case does not compile.
#[test]
fn race_gym_emit_keeps_a_multiline_failure_in_its_comment() {
    let tree = std::env::temp_dir().join(format!("race-gym-emit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tree);
    std::fs::create_dir_all(tree.join("scripts")).unwrap();
    // rustfmt, which the script runs last, reads the module the case section declares.
    std::fs::create_dir_all(tree.join("tests/race_gym")).unwrap();
    std::fs::write(tree.join("tests/race_gym/mod.rs"), "").unwrap();
    let script = tree.join("scripts/race-gym-emit.sh");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/race-gym-emit.sh"),
        &script,
    )
    .unwrap();
    let tests = tree.join("tests/test_lifecycle_interleave.rs");
    std::fs::write(&tests, "use anyhow::Result;\n").unwrap();
    let replay = tree.join("replay.json");
    let body = serde_json::json!({
        "format": 1,
        "scenario": "toy",
        "fcvm_rev": "test",
        "params": {},
        "schedule": "lockstep",
        "steps": [{"actor": 0, "release": "a.one#1", "event": {"exited": 0}}],
        "failed": [{"oracle": "control", "actor": null, "detail": "first line\nsecond line"}],
        "pin_oracles": [],
    });
    std::fs::write(&replay, body.to_string()).unwrap();
    let out = std::process::Command::new("bash")
        .arg(&script)
        .arg(&replay)
        .arg("multiline")
        .output()
        .expect("running race-gym-emit.sh");
    let emitted = std::fs::read_to_string(&tests).unwrap();
    assert!(
        out.status.success(),
        "race-gym-emit.sh failed ({}): {}\n{emitted}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        emitted.contains("/// Recorded failure: control: first line second line\n"),
        "{emitted}"
    );
    std::fs::remove_dir_all(&tree).unwrap();
}
