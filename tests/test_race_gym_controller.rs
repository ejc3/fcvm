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
    let root = std::env::temp_dir().join(format!("race-gym-toy-{}", std::process::id()));
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
    let root = std::env::temp_dir().join(format!("race-gym-silent-{}", std::process::id()));
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
    let root = std::env::temp_dir().join(format!("race-gym-two-{}", std::process::id()));
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
