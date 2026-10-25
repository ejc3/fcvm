//! `cold-run`: N concurrent `fcvm podman run` on one content key that has no snapshot.
//!
//! Every actor runs the same image and command; the command carries the run's tag
//! (`sh -c 'exit 0' gym-<tag>`), which the snapshot key hashes and the guest ignores, so
//! the actors of one run share a key and different runs share none. Each container exits
//! 0, so a healthy actor exits 0. With `--gpu` (or an NV2 profile) a cold run creates the
//! pre-start snapshot, tears its VM down and restores the snapshot, which is the path
//! where the actors' teardowns and restores meet.
//!
//! Host-specific inputs come from the environment, not from a replay: `RACE_GYM_GPUS`
//! (comma list of vfio-pci SBDFs, handed out per run slot and actor) and
//! `RACE_GYM_FCVM` (the fcvm binary, default `./target/release/fcvm`), and `RACE_GYM_IMAGE`,
//! which replaces the image a replay recorded.

use super::{error_lines, Failure, RunCtx, Scenario};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Params {
    pub actors: usize,
    pub image: String,
    /// Extra `podman run` arguments, the same for every actor (`--kernel-profile gpu`).
    pub args: Vec<String>,
    /// Give each actor its own `--gpu` from `RACE_GYM_GPUS`.
    pub gpu_per_actor: bool,
}

/// The recorded parameters of a replay, on this host: the image is a host input like
/// `RACE_GYM_GPUS`, so `RACE_GYM_IMAGE` replaces the recorded one when set.
pub fn params_on_host(recorded: &serde_json::Value, image: Option<&str>) -> Result<Params> {
    let mut params: Params = serde_json::from_value(recorded.clone())?;
    if let Some(image) = image {
        params.image = image.to_string();
    }
    Ok(params)
}

pub struct ColdRun {
    pub params: Params,
    fcvm: PathBuf,
    gpus: Vec<String>,
    /// VM ids seen in state files during each run (by tag), for the leak oracle.
    seen_vms: Mutex<HashMap<String, HashSet<String>>>,
}

pub const NAME: &str = "cold-run";

impl ColdRun {
    pub fn new(params: Params) -> Result<Self> {
        let fcvm = std::env::var("RACE_GYM_FCVM")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("./target/release/fcvm"));
        if !fcvm.exists() {
            bail!(
                "fcvm binary {} not found (make build, or set RACE_GYM_FCVM)",
                fcvm.display()
            );
        }
        let gpus: Vec<String> = std::env::var("RACE_GYM_GPUS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if params.gpu_per_actor && gpus.len() < params.actors {
            bail!(
                "cold-run with a GPU per actor needs {} GPUs in RACE_GYM_GPUS, found {}",
                params.actors,
                gpus.len()
            );
        }
        Ok(Self {
            params,
            fcvm: std::fs::canonicalize(&fcvm)?,
            gpus,
            seen_vms: Mutex::new(HashMap::new()),
        })
    }

    /// How many runs can go at once: each needs its own GPUs.
    pub fn max_parallel(&self) -> usize {
        if self.params.gpu_per_actor {
            self.gpus.len() / self.params.actors
        } else {
            usize::MAX
        }
    }

    fn name_prefix(run: &RunCtx) -> String {
        format!("gym-{}-", run.tag)
    }

    fn argv(&self, run: &RunCtx, idx: usize, name: &str) -> Vec<String> {
        let mut argv = vec![
            "podman".to_string(),
            "run".to_string(),
            "--name".to_string(),
            name.to_string(),
            "--network".to_string(),
            "rootless".to_string(),
        ];
        argv.extend(self.params.args.iter().cloned());
        if self.params.gpu_per_actor {
            argv.push("--gpu".to_string());
            argv.push(self.gpus[run.slot * self.params.actors + idx].clone());
        }
        argv.push(self.params.image.clone());
        argv.extend(["sh", "-c", "exit 0"].map(String::from));
        argv.push(format!("gym-{}", run.tag));
        argv
    }

    /// State files of this run's VMs: (path, state).
    fn states(run: &RunCtx) -> Vec<(PathBuf, fcvm::state::VmState)> {
        let prefix = Self::name_prefix(run);
        let Ok(rd) = std::fs::read_dir(fcvm::paths::state_dir()) else {
            return Vec::new();
        };
        rd.flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .filter_map(|p| {
                let state: fcvm::state::VmState =
                    serde_json::from_str(&std::fs::read_to_string(&p).ok()?).ok()?;
                state
                    .name
                    .as_deref()
                    .is_some_and(|n| n.starts_with(&prefix))
                    .then_some((p, state))
            })
            .collect()
    }

    /// The content key the actors used, from the cache lookup's log line.
    fn snapshot_key(&self, run: &RunCtx) -> Option<String> {
        (0..self.params.actors).find_map(|idx| {
            let log = std::fs::read_to_string(run.actor_log(idx)).ok()?;
            log.lines()
                .filter(|l| l.contains("snapshot_key="))
                .find_map(|l| {
                    let rest = l.split("snapshot_key=").nth(1)?;
                    let key: String = rest.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
                    (key.len() >= 8).then_some(key)
                })
        })
    }

    fn artifact_failures(key: &str) -> Vec<Failure> {
        let dir = fcvm::paths::snapshot_dir().join(key);
        let mut failed = Vec::new();
        match std::fs::read_to_string(dir.join("config.json")) {
            Ok(text) => match serde_json::from_str::<fcvm::storage::SnapshotConfig>(&text) {
                Ok(config) if config.name == key => {}
                Ok(config) => failed.push(Failure::new(
                    "artifact",
                    None,
                    format!(
                        "{}/config.json names {}, not the key",
                        dir.display(),
                        config.name
                    ),
                )),
                Err(e) => failed.push(Failure::new(
                    "artifact",
                    None,
                    format!("{}/config.json does not parse: {e}", dir.display()),
                )),
            },
            Err(e) => failed.push(Failure::new(
                "artifact",
                None,
                format!("{}/config.json: {e}", dir.display()),
            )),
        }
        for file in ["memory.bin", "vmstate.bin", "disk/disk.raw"] {
            if !dir.join(file).exists() {
                failed.push(Failure::new(
                    "artifact",
                    None,
                    format!("{} is missing", dir.join(file).display()),
                ));
            }
        }
        failed
    }

    /// One more run on the key, unarmed: the generation the actors left must restore.
    async fn fresh_restore(&self, run: &RunCtx) -> Result<Option<Failure>> {
        let name = format!("{}fresh", Self::name_prefix(run));
        let log_path = run.log_dir.join(format!("{}-fresh.log", run.tag));
        let log = std::fs::File::create(&log_path)?;
        let mut cmd = tokio::process::Command::new(&self.fcvm);
        cmd.args(self.argv(run, 0, &name))
            .env("RUST_LOG", "info")
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true);
        let mut child = super::spawn_recorded(&mut cmd).context("spawning the fresh restore")?;
        match tokio::time::timeout(Duration::from_secs(300), child.wait()).await {
            Ok(status) => {
                let status = status?;
                if status.success() {
                    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
                    if log.contains("Snapshot miss") {
                        return Ok(Some(Failure::new(
                            "artifact",
                            None,
                            "the fresh run after the schedule missed the cache: no usable generation was left",
                        )));
                    }
                    Ok(None)
                } else {
                    Ok(Some(Failure::new(
                        "artifact",
                        None,
                        format!(
                            "a fresh restore of the key failed ({status}): {}",
                            error_lines(&log_path, 2)
                        ),
                    )))
                }
            }
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                Ok(Some(Failure::new(
                    "artifact",
                    None,
                    "a fresh restore of the key hung for 300 s",
                )))
            }
        }
    }
}

fn path_exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

impl Scenario for ColdRun {
    fn name(&self) -> &str {
        NAME
    }

    fn actors(&self) -> usize {
        self.params.actors
    }

    fn params(&self) -> serde_json::Value {
        serde_json::to_value(&self.params).unwrap()
    }

    fn command(&self, run: &RunCtx, idx: usize) -> Result<tokio::process::Command> {
        let name = format!("{}a{idx}", Self::name_prefix(run));
        let mut cmd = tokio::process::Command::new(&self.fcvm);
        // The oracles read the snapshot key and the errors from INFO lines.
        cmd.args(self.argv(run, idx, &name)).env("RUST_LOG", "info");
        Ok(cmd)
    }

    /// Oracle 4 at every step: every state file of this run names a live process whose
    /// start time it recorded.
    fn step_oracles(&self, run: &RunCtx) -> Vec<Failure> {
        let mut failed = Vec::new();
        for (path, state) in Self::states(run) {
            self.seen_vms
                .lock()
                .unwrap()
                .entry(run.tag.clone())
                .or_default()
                .insert(state.vm_id.clone());
            let Some(pid) = state.pid else { continue };
            let live = fcvm::utils::process_start_time(pid);
            let consistent = match (live, state.pid_start_time) {
                (Some(now), Some(recorded)) => now == recorded,
                (Some(_), None) => true,
                (None, _) => false,
            };
            // A process deleting its own state as it exits is not stale: re-read first.
            if !consistent && path_exists(&path) {
                failed.push(Failure::new(
                    "state",
                    None,
                    format!(
                        "state file {} names pid {pid}, which is {}",
                        path.display(),
                        if live.is_none() {
                            "not running"
                        } else {
                            "another process"
                        }
                    ),
                ));
            }
        }
        failed
    }

    fn end_oracles<'a>(
        &'a self,
        run: &'a RunCtx,
        exits: &'a [i32],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<Failure>> + Send + 'a>> {
        Box::pin(async move {
            let mut failed = Vec::new();
            // 1. Outcome: every actor came up and its container exited 0.
            for (idx, code) in exits.iter().enumerate() {
                if *code != 0 {
                    failed.push(Failure::new(
                        "outcome",
                        Some(idx),
                        format!("exit {code}: {}", error_lines(&run.actor_log(idx), 2)),
                    ));
                }
            }
            // 3. Leaks, read before the fresh restore adds its own VM.
            let orphans = super::orphans();
            if !orphans.is_empty() {
                failed.push(Failure::new(
                    "leak",
                    None,
                    format!("orphaned processes {orphans:?}"),
                ));
            }
            for (path, state) in Self::states(run) {
                failed.push(Failure::new(
                    "leak",
                    None,
                    format!(
                        "state file {} for {:?} outlived its VM",
                        path.display(),
                        state.name
                    ),
                ));
                let _ = std::fs::remove_file(&path);
            }
            let seen = self
                .seen_vms
                .lock()
                .unwrap()
                .remove(&run.tag)
                .unwrap_or_default();
            let mut seen: Vec<_> = seen.into_iter().collect();
            seen.sort();
            for vm_id in &seen {
                let dir = fcvm::paths::vm_runtime_dir(vm_id);
                if path_exists(&dir) {
                    failed.push(Failure::new(
                        "leak",
                        None,
                        format!("VM directory vm-disks/{vm_id} outlived its VM"),
                    ));
                    let _ = std::fs::remove_dir_all(&dir);
                }
            }
            // 2. Artifacts, and that the generation restores.
            match self.snapshot_key(run) {
                Some(key) => {
                    let artifacts = Self::artifact_failures(&key);
                    let complete = artifacts.is_empty();
                    failed.extend(artifacts);
                    if complete {
                        match self.fresh_restore(run).await {
                            Ok(Some(f)) => failed.push(f),
                            Ok(None) => {}
                            Err(e) => failed.push(Failure::new("artifact", None, format!("{e:#}"))),
                        }
                    }
                    let manager = fcvm::storage::SnapshotManager::new(fcvm::paths::snapshot_dir());
                    for name in [key.clone(), format!("{key}-startup")] {
                        let _ = manager.delete_snapshot(&name).await;
                    }
                }
                None => failed.push(Failure::new(
                    "artifact",
                    None,
                    "no actor logged a snapshot key, so no generation can be checked",
                )),
            }
            // Clean up after the fresh restore: it can recreate a source directory again.
            for vm_id in &seen {
                let _ = std::fs::remove_dir_all(fcvm::paths::vm_runtime_dir(vm_id));
            }
            // The fresh restore's VM is gone too.
            for (path, state) in Self::states(run) {
                failed.push(Failure::new(
                    "leak",
                    None,
                    format!(
                        "state file {} for {:?} outlived its VM",
                        path.display(),
                        state.name
                    ),
                ));
                let _ = std::fs::remove_file(&path);
            }
            failed
        })
    }
}
