//! `make race-gym`: run a race gym scenario's schedules, or replay one (`docs/race-gym.md`).
//!
//! Knobs (the Makefile maps its variables onto these):
//!
//! - `RACE_GYM_SCENARIO` (`cold-run`), `RACE_GYM_PHASE` (`pairs` or `walk`, default
//!   `pairs`), `RACE_GYM_SCHEDULES` (at most this many schedules; default: every pair, or
//!   200 walks), `RACE_GYM_FIRST` (skip the first this many pair schedules, so
//!   `FIRST=348 SCHEDULES=1` runs schedule 348 alone), `RACE_GYM_SEED` (first walk seed, default 1), `RACE_GYM_DEPTH` (walk depth,
//!   default 3), `RACE_GYM_PARALLEL` (runs at once, default 1).
//! - `RACE_GYM_IMAGE` (default `alpine:latest`), `RACE_GYM_ARGS` (extra `podman run`
//!   arguments), `RACE_GYM_GPUS` (one `--gpu` per actor when set).
//! - `RACE_GYM_REPLAY=<file>`: rerun that replay instead; with `RACE_GYM_RERECORD=1`, run
//!   its schedule again and write the new replay.
//! - `RACE_GYM_OUT` (default `/tmp/race-gym`): replays of failing schedules and
//!   `summary.json` go under `<out>/<run id>/`.
//!
//! The test fails when any schedule failed an oracle, after writing a replay for each.
//! A failing schedule found with `RACE_GYM_PARALLEL` above 1 is replayed alone before it is
//! reported.

#![cfg(feature = "race-gym")]

#[path = "race_gym/mod.rs"]
mod race_gym;

use anyhow::{bail, Context, Result};
use race_gym::cold_run::{ColdRun, Params};
use race_gym::{Failure, Limits, Plan, Replay, RunCtx, RunResult, Scenario, Schedule};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn env_num<T: std::str::FromStr>(name: &str, default: T) -> Result<T> {
    match env(name) {
        Some(v) => v
            .parse()
            .map_err(|_| anyhow::anyhow!("{name}={v} is not a number")),
        None => Ok(default),
    }
}

fn cold_run(actors: usize) -> Result<ColdRun> {
    let args: Vec<String> = env("RACE_GYM_ARGS")
        .map(|a| shell_words::split(&a))
        .transpose()?
        .unwrap_or_default();
    ColdRun::new(Params {
        actors,
        image: env("RACE_GYM_IMAGE").unwrap_or_else(|| "alpine:latest".to_string()),
        args,
        gpu_per_actor: env("RACE_GYM_GPUS").is_some(),
    })
}

fn ctx(out: &Path, tag: String, slot: usize) -> RunCtx {
    RunCtx {
        park_dir: out.join(&tag).join("park"),
        log_dir: out.join(&tag),
        tag,
        slot,
    }
}

fn record(scenario: &dyn Scenario, schedule: &Schedule, result: &RunResult) -> Replay {
    Replay {
        format: race_gym::FORMAT,
        scenario: scenario.name().to_string(),
        fcvm_rev: race_gym::git_rev(),
        params: scenario.params(),
        schedule: schedule.clone(),
        steps: result.steps.clone(),
        failed: result.failed.clone(),
        pin_oracles: Vec::new(),
    }
}

fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    std::fs::write(path, serde_json::to_string_pretty(value)? + "\n")
        .with_context(|| format!("writing {}", path.display()))
}

fn print_result(label: &str, result: &RunResult) {
    let verdict = if result.failed.is_empty() {
        "pass"
    } else {
        "FAIL"
    };
    println!(
        "RACE-GYM {label}: {verdict} steps={} feasible={} elapsed_ms={}",
        result.steps.len(),
        result.feasible,
        result.elapsed.as_millis()
    );
    for f in &result.failed {
        println!("RACE-GYM   {}", f.signature());
    }
}

async fn replay_file(path: &Path, out: &Path) -> Result<()> {
    let replay: Replay = serde_json::from_str(&std::fs::read_to_string(path)?)
        .with_context(|| format!("parsing {}", path.display()))?;
    let scenario = race_gym::scenario_from(&replay.scenario, &replay.params)?;
    let tag = format!("replay{}", std::process::id());
    let ctx = ctx(out, tag, 0);
    if env("RACE_GYM_RERECORD").is_some() {
        // Run the recorded schedule again and write what it does now.
        let result = race_gym::run(
            scenario.as_ref(),
            &ctx,
            &Limits::default(),
            Plan::Schedule(&replay.schedule),
        )
        .await?;
        print_result(&format!("rerecord {}", path.display()), &result);
        // A re-recording that did not reach the schedule's window would make a case that
        // passes without exercising it.
        if !result.feasible || result.failed.iter().any(|f| f.oracle == "control") {
            bail!(
                "RERECORD refused: the schedule was infeasible ({}) or did not run as planned ({:?})",
                !result.feasible,
                result.failed
            );
        }
        let file = out.join(path.file_name().context("replay path has no file name")?);
        let mut new = record(scenario.as_ref(), &replay.schedule, &result);
        new.pin_oracles = replay.pin_oracles.clone();
        write_json(&file, &new)?;
        println!("RACE-GYM rerecorded: {}", file.display());
        return Ok(());
    }
    let result = race_gym::run(
        scenario.as_ref(),
        &ctx,
        &Limits::default(),
        Plan::Replay(&replay.steps),
    )
    .await?;
    print_result(&format!("replay {}", path.display()), &result);
    println!("RACE-GYM logs: {}", ctx.log_dir.display());
    let recorded: Vec<String> = replay.failed.iter().map(Failure::signature).collect();
    let now: Vec<String> = result.failed.iter().map(Failure::signature).collect();
    if result.failed.is_empty() {
        println!("RACE-GYM replay passes: the recorded failure {recorded:?} no longer happens");
        return Ok(());
    }
    if now == recorded {
        bail!("REPRODUCED: the replay fails as recorded: {now:?}");
    }
    bail!("the replay fails differently: recorded {recorded:?}, now {now:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn race_gym() -> Result<()> {
    race_gym::become_subreaper()?;
    let run_id = format!("{}-{}", chrono_stamp(), std::process::id());
    let out =
        PathBuf::from(env("RACE_GYM_OUT").unwrap_or_else(|| "/tmp/race-gym".into())).join(&run_id);
    std::fs::create_dir_all(&out)?;
    println!("RACE-GYM output: {}", out.display());

    if let Some(file) = env("RACE_GYM_REPLAY") {
        return replay_file(Path::new(&file), &out).await;
    }

    let scenario_name = env("RACE_GYM_SCENARIO").unwrap_or_else(|| "cold-run".into());
    if scenario_name != race_gym::cold_run::NAME {
        bail!("unknown scenario {scenario_name:?} (scenarios: cold-run)");
    }
    let phase = env("RACE_GYM_PHASE").unwrap_or_else(|| "pairs".into());
    let parallel: usize = env_num("RACE_GYM_PARALLEL", 1)?;
    let limits = Arc::new(Limits::default());

    // Discovery: one lock-step run of two actors gives each actor's trace.
    let pair_scenario = Arc::new(cold_run(2)?);
    let discovery = race_gym::run(
        pair_scenario.as_ref(),
        &ctx(&out, format!("{run_id}-d"), 0),
        &limits,
        Plan::Schedule(&Schedule::Lockstep),
    )
    .await?;
    print_result("discovery", &discovery);
    for (i, t) in discovery.traces.iter().enumerate() {
        println!("RACE-GYM trace a{i} ({} hits): {}", t.len(), t.join(" "));
    }
    if discovery.traces.iter().any(|t| t.is_empty()) {
        bail!("discovery recorded an actor with no hits: the park spec did not reach fcvm");
    }

    let first: usize = env_num("RACE_GYM_FIRST", 0)?;
    let (scenario, schedules): (Arc<ColdRun>, Vec<(usize, Schedule)>) = match phase.as_str() {
        "pairs" => {
            let all = race_gym::pair_schedules(&discovery.traces[0], &discovery.traces[1]);
            let limit = env_num("RACE_GYM_SCHEDULES", all.len())?;
            let total = all.len();
            let picked: Vec<_> = all
                .into_iter()
                .enumerate()
                .skip(first)
                .take(limit)
                .collect();
            println!(
                "RACE-GYM pair schedules: {total} (running {} from {first})",
                picked.len()
            );
            (pair_scenario.clone(), picked)
        }
        "walk" => {
            let actors = 4;
            let count = env_num("RACE_GYM_SCHEDULES", 200)?;
            let seed: u64 = env_num("RACE_GYM_SEED", 1)?;
            let depth: u32 = env_num("RACE_GYM_DEPTH", 3)?;
            let hint = discovery.steps.len() * actors / 2;
            let walks = (0..count)
                .map(|n| {
                    let walk = Schedule::Walk {
                        seed: seed + n as u64,
                        depth,
                        steps_hint: hint,
                    };
                    (n, walk)
                })
                .collect();
            (Arc::new(cold_run(actors)?), walks)
        }
        other => bail!("RACE_GYM_PHASE={other}: expected pairs or walk"),
    };
    let parallel = parallel.clamp(1, scenario.max_parallel().max(1));

    // Run the schedules, `parallel` at a time; slot s owns GPU group s.
    let mut results: Vec<(usize, Schedule, RunResult)> = Vec::new();
    let mut queue: Vec<(usize, Schedule)> = schedules;
    queue.reverse();
    let mut running = tokio::task::JoinSet::new();
    let mut free: Vec<usize> = (0..parallel).rev().collect();
    loop {
        while let (Some(slot), Some((n, schedule))) = (free.last().copied(), queue.last().cloned())
        {
            free.pop();
            queue.pop();
            let (scenario, limits, out, tag) = (
                scenario.clone(),
                limits.clone(),
                out.clone(),
                format!("{run_id}-{n}"),
            );
            running.spawn(async move {
                let ctx = ctx(&out, tag, slot);
                let result =
                    race_gym::run(scenario.as_ref(), &ctx, &limits, Plan::Schedule(&schedule))
                        .await;
                (slot, n, schedule, result)
            });
        }
        let Some(done) = running.join_next().await else {
            break;
        };
        let (slot, n, schedule, result) = done?;
        free.push(slot);
        let result = result?;
        print_result(&format!("schedule {n} {schedule:?}"), &result);
        append_result(&out, n, &schedule, &result)?;
        results.push((n, schedule, result));
    }
    results.sort_by_key(|(n, _, _)| *n);

    // Group failures by signature. For each signature, one schedule is enough: replay it
    // alone when the runs were parallel (a failure that does not reproduce alone may come
    // from a neighbouring run, not from the schedule), and write its replay.
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (n, _, result) in &results {
        for f in &result.failed {
            let ns = groups.entry(f.signature()).or_default();
            if ns.last() != Some(n) {
                ns.push(*n);
            }
        }
    }
    let mut files: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut unconfirmed: Vec<String> = Vec::new();
    for (k, (signature, ns)) in groups.iter().enumerate() {
        // A control failure (unmodeled wait, divergence, a park that timed out) says the
        // schedule did not run as recorded. It is reported as it is, never dropped as
        // "not reproduced alone": a replay of a schedule that did not complete cannot agree.
        let control = signature.starts_with("control:");
        for &n in ns.iter().take(3) {
            let (_, schedule, result) =
                &results[results.iter().position(|(m, _, _)| *m == n).unwrap()];
            let mut replay = record(scenario.as_ref(), schedule, result);
            if parallel > 1 && !control {
                let confirm = race_gym::run(
                    scenario.as_ref(),
                    &ctx(&out, format!("{run_id}-{n}-confirm{k}"), 0),
                    &limits,
                    Plan::Replay(&result.steps),
                )
                .await?;
                print_result(&format!("schedule {n} serial replay"), &confirm);
                if !confirm.failed.iter().any(|f| f.signature() == *signature) {
                    println!("RACE-GYM schedule {n}: {signature:?} not reproduced alone");
                    continue;
                }
                replay.failed = confirm.failed;
            }
            let file = out.join(format!("{}-{n}.json", scenario.name()));
            write_json(&file, &replay)?;
            files.insert(signature.clone(), file);
            break;
        }
        if !files.contains_key(signature) {
            unconfirmed.push(signature.clone());
        }
    }

    let elapsed: Vec<u128> = results
        .iter()
        .map(|(_, _, r)| r.elapsed.as_millis())
        .collect();
    let mut sorted = elapsed.clone();
    sorted.sort_unstable();
    let median = sorted.get(sorted.len() / 2).copied().unwrap_or(0);
    let infeasible = results.iter().filter(|(_, _, r)| !r.feasible).count();
    let summary = serde_json::json!({
        "scenario": scenario.name(),
        "phase": phase,
        "fcvm_rev": race_gym::git_rev(),
        "params": scenario.params(),
        "parallel": parallel,
        "discovery_traces": discovery.traces,
        "schedules": results.len(),
        "infeasible": infeasible,
        "elapsed_ms": { "median": median, "min": sorted.first(), "max": sorted.last(),
                        "total": elapsed.iter().sum::<u128>() },
        "failures": groups.iter().map(|(sig, ns)| serde_json::json!({
            "signature": sig, "count": ns.len(), "schedules": ns, "replay": files.get(sig) }))
            .collect::<Vec<_>>(),
        "not_reproduced_alone": unconfirmed,
    });
    write_json(&out.join("summary.json"), &summary)?;
    println!(
        "RACE-GYM summary: {}",
        serde_json::to_string_pretty(&summary)?
    );
    if !files.is_empty() {
        bail!(
            "{} distinct failures, each with a replay in {}",
            files.len(),
            out.display()
        );
    }
    Ok(())
}

/// One line per finished schedule, so a run that is stopped keeps what it found.
fn append_result(out: &Path, n: usize, schedule: &Schedule, result: &RunResult) -> Result<()> {
    use std::io::Write;
    let line = serde_json::json!({
        "n": n,
        "schedule": schedule,
        "feasible": result.feasible,
        "steps": result.steps.len(),
        "elapsed_ms": result.elapsed.as_millis() as u64,
        "failed": result.failed.iter().map(Failure::signature).collect::<Vec<_>>(),
    });
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(out.join("results.jsonl"))?;
    writeln!(file, "{line}")?;
    Ok(())
}

fn chrono_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}
