//! Warm GPU snapshots: parking the guest's GPUs so the VM can be snapshotted.
//!
//! Firecracker cannot save a passthrough device, so a VM is snapshotted without
//! one. To keep what a CUDA process has on the GPU (a model's weights, its KV
//! cache, compiled kernels), `fc-agent --gpu-park` moves it into the process's
//! own memory first, with NVIDIA's cuda-checkpoint (driver API
//! `cuCheckpointProcess*`), and then lets go of the GPU:
//!
//! 1. lock every process that holds an NVIDIA device open, then checkpoint it:
//!    the driver copies its device memory to host memory and closes its GPU
//!    handles;
//! 2. stop nvidia-persistenced, the last holder;
//! 3. remove each GPU from the guest's PCI bus (`/sys/bus/pci/devices/<addr>/remove`),
//!    so the guest has no device for the host to take away;
//! 4. write the [`Parked`] record to [`PARKED_RECORD`] (on /run, which is in guest
//!    memory, so the snapshot carries it).
//!
//! The host then detaches the GPUs from the VMM and takes the snapshot. Undoing
//! it ([`unpark`]) is the same in the source VM (`fc-agent --gpu-unpark`, after the
//! host attaches the GPUs again) and in a clone (from the restore's GPU attach):
//! bring the GPUs up as for any restore, then restore each checkpointed process
//! onto them, mapping the snapshot's GPU UUIDs to the ones it now has, and unlock it.
//!
//! The NVIDIA kernel modules stay loaded across the park: unloading them would
//! change nothing a clone needs, and the container's device nodes keep their
//! numbers.
//!
//! A process that holds the GPU through a graphics API rather than CUDA (an Android
//! Emulator's renderer) cannot be checkpointed. Its container names a hook in the
//! [`GPU_HOOK_LABEL`] image label: park runs it with `release` first, and the hook
//! lets go of the GPU (no `/dev/nvidia*` descriptor or mapping left) while its
//! process keeps running; unpark runs it with `retake` once the GPUs are up and the
//! CUDA processes are restored.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::gpu;

/// The record of a park. /run is a tmpfs, so it is in the snapshot.
pub const PARKED_RECORD: &str = "/run/fcvm-gpu/parked.json";

/// nvidia-persistenced's pid file (its default `--run-dir` is /var/run/nvidia-persistenced).
const PERSISTENCED_PID_FILE: &str = "/var/run/nvidia-persistenced/nvidia-persistenced.pid";

/// A lock waits for the process's submitted CUDA work to finish.
const LOCK_TIMEOUT_MS: u32 = 30_000;
/// One cuda-checkpoint call: a checkpoint or restore copies the process's device
/// memory once (a few GB on a T4 is a fraction of a second over PCIe).
const CHECKPOINT_TIMEOUT: Duration = Duration::from_secs(300);
/// How long nvidia-persistenced may take to exit after SIGTERM.
const PERSISTENCED_EXIT_TIMEOUT: Duration = Duration::from_secs(10);

/// The image label naming the container's GPU hook: an executable in the container
/// that `release` makes let go of the GPU and `retake` makes take it up again.
pub const GPU_HOOK_LABEL: &str = "dev.fcvm.gpu-hook";
/// One hook call. The emulator's release and retake each measure under a second on a
/// T4; the rest is the margin a cold page cache and a busy guest need.
const GPU_HOOK_TIMEOUT: Duration = Duration::from_secs(60);
/// The container fc-agent runs.
const CONTAINER: &str = "fcvm-container";

/// What a park did, so it can be undone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parked {
    /// The checkpointed processes (guest pids), in the order they were checkpointed.
    pub pids: Vec<u32>,
    /// The parked GPUs' UUIDs, in `nvidia-smi -L` order.
    pub uuids: Vec<String>,
    /// The guest PCI addresses the GPUs were removed from.
    pub addresses: Vec<String>,
    /// The container's GPU hook, which released the GPU and must retake it.
    #[serde(default)]
    pub hook: Option<String>,
}

impl Parked {
    /// The record at `path`, or `None` when there is none.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(
                serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing {}", path.display()))?,
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn store(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))
    }
}

/// The `--device-map` for cuda-checkpoint's restore: `old=new` for every GPU, or
/// `None` when every GPU kept its UUID. The parked and current lists are each
/// sorted, so a clone given other GPUs of the same count maps them in a fixed order.
pub fn device_map(parked: &[String], current: &[String]) -> Result<Option<String>> {
    if parked.len() != current.len() {
        bail!(
            "the snapshot's processes were checkpointed on {} GPU(s) ({}) and this VM has {} ({})",
            parked.len(),
            parked.join(", "),
            current.len(),
            current.join(", ")
        );
    }
    let mut old = parked.to_vec();
    let mut new = current.to_vec();
    old.sort();
    new.sort();
    if old == new {
        return Ok(None);
    }
    Ok(Some(
        old.iter()
            .zip(&new)
            .map(|(o, n)| format!("{o}={n}"))
            .collect::<Vec<_>>()
            .join(","),
    ))
}

/// Every process under `proc_root` with a file descriptor open on an NVIDIA device
/// (`/dev/nvidia*`), with those paths, leaving out the pids in `except`.
pub fn nvidia_device_holders(proc_root: &Path, except: &[u32]) -> BTreeMap<u32, Vec<String>> {
    let mut holders = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return holders;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if except.contains(&pid) {
            continue;
        }
        // A process can exit while it is read; its directory then reads as empty.
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        let mut paths: Vec<String> = fds
            .flatten()
            .filter_map(|fd| std::fs::read_link(fd.path()).ok())
            .map(|target| target.to_string_lossy().into_owned())
            .filter(|target| target.starts_with("/dev/nvidia"))
            .collect();
        if !paths.is_empty() {
            paths.sort();
            paths.dedup();
            holders.insert(pid, paths);
        }
    }
    holders
}

fn describe_holders(holders: &BTreeMap<u32, Vec<String>>) -> String {
    holders
        .iter()
        .map(|(pid, paths)| {
            let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
                .map(|c| c.trim().to_string())
                .unwrap_or_else(|_| "?".to_string());
            format!("{pid} ({comm}): {}", paths.join(" "))
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn podman(args: &[&str]) -> tokio::process::Command {
    let mut argv: Vec<String> = crate::container::podman_cmd_prefix().to_vec();
    argv.push("podman".to_string());
    argv.extend(args.iter().map(|a| a.to_string()));
    let mut command = tokio::process::Command::new(&argv[0]);
    command.args(&argv[1..]);
    command
}

/// The GPU hook the running container's image names in [`GPU_HOOK_LABEL`], if any.
/// No container running means no hook; a container podman cannot inspect is an error,
/// since a hook that is not run leaves its process holding the GPU.
async fn container_gpu_hook() -> Result<Option<String>> {
    let mut exists = podman(&["container", "exists", CONTAINER]);
    exists.kill_on_drop(true);
    let status = tokio::time::timeout(GPU_HOOK_TIMEOUT, exists.status())
        .await
        .map_err(|_| anyhow::anyhow!("podman container exists did not finish"))?
        .context("starting podman container exists")?;
    if status.code() == Some(1) {
        return Ok(None);
    }
    let format = format!("{{{{index .Config.Labels \"{GPU_HOOK_LABEL}\"}}}}");
    let label = gpu::run_checked(
        &format!("podman inspect {CONTAINER}"),
        podman(&["inspect", "--format", &format, CONTAINER]),
        GPU_HOOK_TIMEOUT,
    )
    .await?;
    Ok(parse_hook_label(&label))
}

/// `podman inspect`'s rendering of the label: empty or `<no value>` when unset.
fn parse_hook_label(rendered: &str) -> Option<String> {
    let hook = rendered.trim();
    (!hook.is_empty() && hook != "<no value>").then(|| hook.to_string())
}

/// Runs `hook action` in the container and returns what it printed.
async fn run_gpu_hook(hook: &str, action: &str) -> Result<String> {
    gpu::run_checked(
        &format!("the container's GPU hook ({hook} {action})"),
        podman(&["exec", CONTAINER, hook, action]),
        GPU_HOOK_TIMEOUT,
    )
    .await
}

fn cuda_checkpoint(args: &[String]) -> tokio::process::Command {
    let root = Path::new(gpu::DRIVER_ROOT);
    let mut command = tokio::process::Command::new(root.join("usr/bin/cuda-checkpoint"));
    command
        .args(args)
        .env("LD_LIBRARY_PATH", gpu::library_dir(root));
    command
}

async fn run_cuda_checkpoint(args: &[String]) -> Result<String> {
    gpu::run_checked(
        &format!("cuda-checkpoint {}", args.join(" ")),
        cuda_checkpoint(args),
        CHECKPOINT_TIMEOUT,
    )
    .await
}

fn action(action: &str, pid: u32) -> Vec<String> {
    vec![
        "--action".to_string(),
        action.to_string(),
        "--pid".to_string(),
        pid.to_string(),
    ]
}

async fn checkpoint_state(pid: u32) -> Result<String> {
    let out = run_cuda_checkpoint(&[
        "--get-state".to_string(),
        "--pid".to_string(),
        pid.to_string(),
    ])
    .await?;
    Ok(out.trim().to_string())
}

fn persistenced_pid() -> Option<u32> {
    std::fs::read_to_string(PERSISTENCED_PID_FILE)
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

async fn stop_persistenced() -> Result<()> {
    let Some(pid) = persistenced_pid().filter(|pid| pid_alive(*pid)) else {
        bail!(
            "nvidia-persistenced is not running ({PERSISTENCED_PID_FILE} names no live \
             process); a GPU VM starts it when the GPU is attached"
        );
    };
    // SAFETY: kill(2) takes no pointers.
    if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("signalling nvidia-persistenced ({pid})"));
    }
    let deadline = Instant::now() + PERSISTENCED_EXIT_TIMEOUT;
    while pid_alive(pid) {
        if Instant::now() >= deadline {
            bail!("nvidia-persistenced ({pid}) did not exit within {PERSISTENCED_EXIT_TIMEOUT:?} of SIGTERM");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

fn remove_function(sysfs: &Path, address: &str) -> Result<()> {
    let remove = sysfs.join("bus/pci/devices").join(address).join("remove");
    std::fs::write(&remove, "1").with_context(|| format!("writing {}", remove.display()))
}

/// Steps of a park that succeeded, undone in reverse when a later one fails.
#[derive(Default)]
struct Done {
    released_by_hook: Option<String>,
    locked: Vec<u32>,
    checkpointed: Vec<u32>,
    persistenced_stopped: bool,
    removed: Vec<String>,
}

async fn undo(done: &Done) -> Vec<String> {
    let mut errors = Vec::new();
    if !done.removed.is_empty() {
        let placeholders = done.removed.clone();
        if let Err(e) = gpu::rescan_for_gpus(Path::new("/sys"), &placeholders).await {
            errors.push(format!("{e:#}"));
        }
    }
    if done.persistenced_stopped {
        if let Err(e) = gpu::start_persistenced(Path::new(gpu::DRIVER_ROOT)).await {
            errors.push(format!("{e:#}"));
        }
    }
    for &pid in done.checkpointed.iter().rev() {
        if let Err(e) = run_cuda_checkpoint(&action("restore", pid)).await {
            errors.push(format!("{e:#}"));
        }
    }
    for &pid in done.locked.iter().rev() {
        if let Err(e) = run_cuda_checkpoint(&action("unlock", pid)).await {
            errors.push(format!("{e:#}"));
        }
    }
    if let Some(hook) = &done.released_by_hook {
        if let Err(e) = run_gpu_hook(hook, "retake").await {
            errors.push(format!("{e:#}"));
        }
    }
    errors
}

/// `fc-agent --gpu-park`. On failure every step already taken is undone, and the
/// error says what was undone and what could not be.
pub async fn park() -> Result<Parked> {
    if let Some(parked) = Parked::load(Path::new(PARKED_RECORD))? {
        bail!(
            "the GPUs are already parked ({} process(es), GPUs {}); unpark them first",
            parked.pids.len(),
            parked.uuids.join(", ")
        );
    }
    let mut done = Done::default();
    match park_steps(&mut done).await {
        Ok(parked) => Ok(parked),
        Err(error) => {
            let undo_errors = undo(&done).await;
            if undo_errors.is_empty() {
                Err(error.context("parking the GPUs failed; every step taken was undone"))
            } else {
                Err(error.context(format!(
                    "parking the GPUs failed, and undoing it also failed: {}",
                    undo_errors.join("; ")
                )))
            }
        }
    }
}

async fn park_steps(done: &mut Done) -> Result<Parked> {
    let started = Instant::now();
    let root = Path::new(gpu::DRIVER_ROOT);
    let sysfs = Path::new("/sys");
    let gpus = gpu::nvidia_gpus(&gpu::read_pci_functions(sysfs)?);
    if gpus.is_empty() {
        bail!("this VM has no NVIDIA GPU to park");
    }
    let mut smi = tokio::process::Command::new(root.join("usr/bin/nvidia-smi"));
    smi.arg("-L").env("LD_LIBRARY_PATH", gpu::library_dir(root));
    let uuids =
        gpu::parse_smi_uuids(&gpu::run_checked("nvidia-smi -L", smi, CHECKPOINT_TIMEOUT).await?);
    if uuids.len() != gpus.len() {
        bail!(
            "the guest bus has {} NVIDIA GPU(s) and nvidia-smi -L lists {} UUID(s)",
            gpus.len(),
            uuids.len()
        );
    }

    // A process holding the GPU outside CUDA lets go of it through its container's
    // hook first; whatever still holds a device afterwards must be a CUDA process.
    let hook = container_gpu_hook().await?;
    let mut release_ms = 0.0;
    if let Some(hook) = &hook {
        // Recorded before the release runs: a hook that released the GPU and then failed
        // or timed out still gets its retake when the park is undone. A retake of a GPU
        // that was never released does nothing.
        done.released_by_hook = Some(hook.clone());
        let release_started = Instant::now();
        let out = run_gpu_hook(hook, "release").await?;
        release_ms = release_started.elapsed().as_secs_f64() * 1000.0;
        eprintln!(
            "[fc-agent] gpu: the container's hook released the GPU in {release_ms:.1} ms: {}",
            out.trim()
        );
    }

    let me = std::process::id();
    let daemon = persistenced_pid();
    let except: Vec<u32> = std::iter::once(me).chain(daemon).collect();
    let holders = nvidia_device_holders(Path::new("/proc"), &except);
    let mut pids = Vec::new();
    for &pid in holders.keys() {
        match checkpoint_state(pid).await {
            Ok(state) if state == "running" => pids.push(pid),
            Ok(state) => bail!(
                "process {pid} holds an NVIDIA device and its CUDA state is {state:?}, not \
                 \"running\"; holders: {}",
                describe_holders(&holders)
            ),
            Err(error) => bail!(
                "process {pid} holds an NVIDIA device but cuda-checkpoint cannot checkpoint it \
                 ({error:#}); every process with a GPU open must be a CUDA process. Holders: {}",
                describe_holders(&holders)
            ),
        }
    }

    let lock_started = Instant::now();
    for &pid in &pids {
        let mut args = action("lock", pid);
        args.extend(["--timeout".to_string(), LOCK_TIMEOUT_MS.to_string()]);
        run_cuda_checkpoint(&args).await?;
        done.locked.push(pid);
    }
    let lock_ms = lock_started.elapsed().as_secs_f64() * 1000.0;
    let checkpoint_started = Instant::now();
    for &pid in &pids {
        run_cuda_checkpoint(&action("checkpoint", pid)).await?;
        done.checkpointed.push(pid);
    }
    let checkpoint_ms = checkpoint_started.elapsed().as_secs_f64() * 1000.0;

    let still = nvidia_device_holders(Path::new("/proc"), &except);
    if !still.is_empty() {
        bail!(
            "after the checkpoint these processes still hold an NVIDIA device: {}",
            describe_holders(&still)
        );
    }
    stop_persistenced().await?;
    done.persistenced_stopped = true;
    let still = nvidia_device_holders(Path::new("/proc"), &[me]);
    if !still.is_empty() {
        bail!(
            "with nvidia-persistenced stopped these processes still hold an NVIDIA device: {}",
            describe_holders(&still)
        );
    }

    let addresses: Vec<String> = gpus.iter().map(|g| g.address.clone()).collect();
    for address in &addresses {
        remove_function(sysfs, address)?;
        done.removed.push(address.clone());
    }
    let left = gpu::nvidia_gpus(&gpu::read_pci_functions(sysfs)?);
    if !left.is_empty() {
        bail!(
            "after removing them the guest bus still shows NVIDIA GPU(s): {}",
            left.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let parked = Parked {
        pids,
        uuids,
        addresses,
        hook,
    };
    parked.store(Path::new(PARKED_RECORD))?;
    eprintln!(
        "[fc-agent] gpu: parked {} GPU(s) ({}), {} process(es) {:?}: hook_release_ms={release_ms:.1} \
         lock_ms={lock_ms:.1} checkpoint_ms={checkpoint_ms:.1} total_ms={:.1}",
        parked.uuids.len(),
        parked.uuids.join(", "),
        parked.pids.len(),
        parked.pids,
        started.elapsed().as_secs_f64() * 1000.0
    );
    Ok(parked)
}

/// Bring the GPUs up again and restore the parked processes onto them.
/// `announced` names the GPUs the host attached (only the count is used).
/// Returns the bring-up's report (the GPUs initialised and each step's duration).
pub async fn unpark(parked: &Parked, announced: &[String]) -> Result<gpu::AttachReport> {
    if announced.len() != parked.uuids.len() {
        bail!(
            "this VM's processes were checkpointed on {} GPU(s) and the host attached {}; a \
             warm GPU snapshot must be restored with as many GPUs as it parked",
            parked.uuids.len(),
            announced.len()
        );
    }
    let started = Instant::now();
    let up = gpu::bring_up(announced, false).await?;
    let up_ms = started.elapsed().as_secs_f64() * 1000.0;
    let map = device_map(&parked.uuids, &up.uuids)?;
    let restore_started = Instant::now();
    for &pid in &parked.pids {
        let mut args = action("restore", pid);
        if let Some(map) = &map {
            args.extend(["--device-map".to_string(), map.clone()]);
        }
        run_cuda_checkpoint(&args)
            .await
            .with_context(|| format!("restoring parked process {pid} onto the GPU"))?;
    }
    let restore_ms = restore_started.elapsed().as_secs_f64() * 1000.0;
    for &pid in &parked.pids {
        run_cuda_checkpoint(&action("unlock", pid))
            .await
            .with_context(|| format!("unlocking parked process {pid}"))?;
    }
    let mut retake_ms = 0.0;
    if let Some(hook) = &parked.hook {
        let retake_started = Instant::now();
        let out = run_gpu_hook(hook, "retake")
            .await
            .context("the container's GPU hook could not take the GPU up again")?;
        retake_ms = retake_started.elapsed().as_secs_f64() * 1000.0;
        eprintln!(
            "[fc-agent] gpu: the container's hook retook the GPU in {retake_ms:.1} ms: {}",
            out.trim()
        );
    }
    let record = PathBuf::from(PARKED_RECORD);
    std::fs::remove_file(&record).with_context(|| format!("removing {}", record.display()))?;
    eprintln!(
        "[fc-agent] gpu: unparked {} process(es) onto {} GPU(s){}: bring_up_ms={up_ms:.1} \
         cuda_restore_ms={restore_ms:.1} hook_retake_ms={retake_ms:.1} total_ms={:.1}",
        parked.pids.len(),
        up.report.gpus,
        if map.is_some() { " (other GPUs)" } else { "" },
        started.elapsed().as_secs_f64() * 1000.0
    );
    Ok(up.report)
}

/// `fc-agent --gpu-unpark`, in the source VM after the host attached its GPUs again.
pub async fn unpark_source() -> Result<()> {
    let Some(parked) = Parked::load(Path::new(PARKED_RECORD))? else {
        bail!("no GPUs are parked ({PARKED_RECORD} does not exist)");
    };
    unpark(&parked, &parked.addresses).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_gpus_need_no_device_map() {
        let gpus = vec!["GPU-b".to_string(), "GPU-a".to_string()];
        let same = vec!["GPU-a".to_string(), "GPU-b".to_string()];
        assert_eq!(device_map(&gpus, &same).unwrap(), None);
    }

    #[test]
    fn other_gpus_are_mapped_in_sorted_order() {
        let parked = vec!["GPU-2".to_string(), "GPU-1".to_string()];
        let now = vec!["GPU-9".to_string(), "GPU-8".to_string()];
        assert_eq!(
            device_map(&parked, &now).unwrap().as_deref(),
            Some("GPU-1=GPU-8,GPU-2=GPU-9")
        );
    }

    #[test]
    fn a_different_gpu_count_is_refused() {
        let err = device_map(&["GPU-1".to_string()], &[]).unwrap_err();
        assert!(format!("{err:#}").contains("1 GPU(s)"), "{err:#}");
    }

    #[test]
    fn holders_are_processes_with_an_nvidia_device_open() {
        let proc_root = tempfile::tempdir().unwrap();
        let mk = |pid: u32, targets: &[&str]| {
            let fd = proc_root.path().join(pid.to_string()).join("fd");
            std::fs::create_dir_all(&fd).unwrap();
            for (i, target) in targets.iter().enumerate() {
                std::os::unix::fs::symlink(target, fd.join(i.to_string())).unwrap();
            }
        };
        mk(
            10,
            &["/dev/nvidia0", "/dev/nvidiactl", "/dev/nvidia0", "/tmp/x"],
        );
        mk(11, &["/dev/null", "socket:[1]"]);
        mk(12, &["/dev/nvidia-uvm"]);
        mk(13, &["/dev/nvidiactl"]);
        std::fs::create_dir_all(proc_root.path().join("self")).unwrap();
        let holders = nvidia_device_holders(proc_root.path(), &[13]);
        assert_eq!(
            holders.into_iter().collect::<Vec<_>>(),
            vec![
                (
                    10,
                    vec!["/dev/nvidia0".to_string(), "/dev/nvidiactl".to_string()]
                ),
                (12, vec!["/dev/nvidia-uvm".to_string()]),
            ]
        );
    }

    #[test]
    fn the_parked_record_round_trips_and_is_absent_until_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpu/parked.json");
        assert_eq!(Parked::load(&path).unwrap(), None);
        let parked = Parked {
            pids: vec![42, 43],
            uuids: vec!["GPU-1".to_string()],
            addresses: vec!["0000:00:06.0".to_string()],
            hook: Some("/usr/local/bin/gpu-hook".to_string()),
        };
        parked.store(&path).unwrap();
        assert_eq!(Parked::load(&path).unwrap(), Some(parked));
    }

    /// A record written before the hook existed loads as parked without one.
    #[test]
    fn a_record_without_a_hook_loads_with_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("parked.json");
        std::fs::write(
            &path,
            r#"{"pids":[7],"uuids":["GPU-1"],"addresses":["0000:00:06.0"]}"#,
        )
        .unwrap();
        assert_eq!(Parked::load(&path).unwrap().unwrap().hook, None);
    }

    #[test]
    fn an_unset_hook_label_is_no_hook() {
        assert_eq!(parse_hook_label("\n"), None);
        assert_eq!(parse_hook_label("<no value>\n"), None);
        assert_eq!(
            parse_hook_label("/usr/local/bin/gpu-hook\n").as_deref(),
            Some("/usr/local/bin/gpu-hook")
        );
    }

    /// The hook releases the GPU before any holder is read or checkpointed (a process
    /// it releases must not be taken for a CUDA process), and retakes it only once the
    /// CUDA processes are restored and unlocked, before the record goes.
    #[test]
    fn the_hook_releases_before_the_holders_are_read_and_retakes_after_the_unlock() {
        let source = include_str!("gpu_park.rs");
        let code = source.split("#[cfg(test)]").next().unwrap();
        let park = &code[code.find("async fn park_steps(").unwrap()..];
        let at =
            |body: &str, needle: &str| body.find(needle).unwrap_or_else(|| panic!("no {needle}"));
        assert!(
            at(park, "run_gpu_hook(hook, \"release\")")
                < at(park, "nvidia_device_holders(Path::new(\"/proc\"), &except)")
        );
        // The retake is registered for the undo before the release runs, so a release
        // that failed after the hook let go of the GPU is still retaken.
        assert!(
            at(park, "done.released_by_hook = Some(") < at(park, "run_gpu_hook(hook, \"release\")")
        );
        let unpark = &code[code.find("pub async fn unpark(").unwrap()..];
        let unlock = at(unpark, "action(\"unlock\", pid)");
        let retake = at(unpark, "run_gpu_hook(hook, \"retake\")");
        let record = at(unpark, "remove_file(&record)");
        assert!(unlock < retake && retake < record);
    }

    /// The park order is fixed: every process is locked before any is
    /// checkpointed, nothing holds a GPU before persistenced stops and before
    /// the functions are removed, and the record is written last.
    #[test]
    fn park_locks_checkpoints_stops_the_daemon_then_removes_the_gpus() {
        let source = include_str!("gpu_park.rs");
        let code = source.split("#[cfg(test)]").next().unwrap();
        let body = &code[code.find("async fn park_steps(").unwrap()..];
        let at = |needle: &str| body.find(needle).unwrap_or_else(|| panic!("no {needle}"));
        let lock = at("action(\"lock\", pid)");
        let checkpoint = at("action(\"checkpoint\", pid)");
        let stop = at("stop_persistenced().await");
        let remove = at("remove_function(sysfs, address)");
        let store = at("parked.store(");
        assert!(lock < checkpoint && checkpoint < stop && stop < remove && remove < store);
    }

    /// Unpark restores every process before unlocking any, and only after the
    /// GPUs are up.
    #[test]
    fn unpark_brings_the_gpus_up_restores_then_unlocks() {
        let source = include_str!("gpu_park.rs");
        let code = source.split("#[cfg(test)]").next().unwrap();
        let body = &code[code.find("pub async fn unpark(").unwrap()..];
        let at = |needle: &str| body.find(needle).unwrap_or_else(|| panic!("no {needle}"));
        let up = at("gpu::bring_up(announced, false)");
        let restore = at("action(\"restore\", pid)");
        let unlock = at("action(\"unlock\", pid)");
        let record = at("remove_file(&record)");
        assert!(up < restore && restore < unlock && unlock < record);
    }

    #[test]
    fn smi_uuids_are_read_in_order() {
        let listed = "GPU 0: Tesla T4 (UUID: GPU-aaa)\nGPU 1: Tesla T4 (UUID: GPU-bbb)\n";
        assert_eq!(
            gpu::parse_smi_uuids(listed),
            vec!["GPU-aaa".to_string(), "GPU-bbb".to_string()]
        );
    }
}
