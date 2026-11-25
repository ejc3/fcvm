//! A VM's start and stop write into no directory that every VM shares and wait on no lock
//! that every VM takes.
//!
//! With every state file in the state directory itself and a host-wide lock around each
//! name claim, clones starting together queued on that lock and on the directory's own
//! lock, which a create or a rename holds exclusively. These tests use only what a launch
//! calls, so they fail on a layout that puts either back.

use fcvm::state::{HealthStatus, StateManager, VmState, VmStatus};
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
use tempfile::TempDir;

/// What a launch and its teardown do to the store.
async fn start_and_stop(manager: &StateManager, vm_id: &str, name: &str) {
    let mut vm = VmState::new(vm_id.to_string(), "alpine:latest".to_string(), 1, 512);
    vm.name = Some(name.to_string());
    manager.claim_name(&mut vm).await.unwrap();
    manager.allocate_loopback_ip(&mut vm).await.unwrap();
    vm.status = VmStatus::Running;
    manager.save_state(&vm).await.unwrap();
    manager
        .update_health_status(vm_id, HealthStatus::Healthy, None)
        .await
        .unwrap();
    manager.delete_state(vm_id).await.unwrap();
}

/// The state directory and the directories directly in it: the ones every VM's start goes
/// through, whatever its vm_id, name and PID.
fn directories_every_vm_shares(state_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut dirs = vec![state_dir.to_path_buf()];
    for entry in std::fs::read_dir(state_dir).unwrap().flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            dirs.push(entry.path());
        }
    }
    dirs
}

/// Once a VM's buckets exist, starting and stopping it creates, renames and deletes nothing
/// in the state directory or in a directory directly inside it. Each of those operations
/// takes its directory's lock exclusively, so one such directory serializes every start on
/// the host.
#[tokio::test]
async fn a_start_and_stop_changes_no_directory_every_vm_shares() {
    let temp_dir = TempDir::new().unwrap();
    let manager = StateManager::new(temp_dir.path().to_path_buf());
    manager.init().await.unwrap();
    // The first start makes the directories this VM's vm_id, name and PID need.
    start_and_stop(&manager, "vm-shared-dir", "shared-dir").await;

    let inotify = Inotify::init(InitFlags::IN_NONBLOCK).unwrap();
    let watched = directories_every_vm_shares(temp_dir.path());
    for dir in &watched {
        inotify
            .add_watch(
                dir,
                AddWatchFlags::IN_CREATE
                    | AddWatchFlags::IN_DELETE
                    | AddWatchFlags::IN_MOVED_FROM
                    | AddWatchFlags::IN_MOVED_TO,
            )
            .unwrap();
    }
    start_and_stop(&manager, "vm-shared-dir", "shared-dir").await;

    let mut changed = Vec::new();
    loop {
        match inotify.read_events() {
            Ok(events) if !events.is_empty() => {
                for event in events {
                    changed.push(format!(
                        "{:?} {}",
                        event.mask,
                        event
                            .name
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "(no name)".into())
                    ));
                }
            }
            Ok(_) | Err(nix::errno::Errno::EAGAIN) => break,
            Err(error) => panic!("reading inotify events: {error}"),
        }
    }
    assert!(
        changed.is_empty(),
        "a start and stop made {} changes in the {} directories every VM shares: {changed:?}",
        changed.len(),
        watched.len()
    );
}

/// A start and stop finish while another process holds every lock file in the state
/// directory itself: none of them is one a start waits for. A lock there is one every VM on
/// the host would take.
#[tokio::test]
async fn a_start_and_stop_waits_on_no_lock_every_vm_shares() {
    use nix::fcntl::{Flock, FlockArg};
    let temp_dir = TempDir::new().unwrap();
    let manager = StateManager::new(temp_dir.path().to_path_buf());
    manager.init().await.unwrap();
    start_and_stop(&manager, "vm-shared-lock", "shared-lock").await;

    let held: Vec<_> = std::fs::read_dir(temp_dir.path())
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".lock"))
        .map(|entry| {
            let file = std::fs::File::open(entry.path()).unwrap();
            (
                entry.file_name(),
                Flock::lock(file, FlockArg::LockExclusive).unwrap(),
            )
        })
        .collect();
    let names: Vec<_> = held.iter().map(|(name, _)| name.clone()).collect();

    // On a thread and a runtime of its own: a start that blocks in flock never returns to
    // its runtime, so a timer on that runtime could not fire and the test would hang.
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(start_and_stop(&manager, "vm-shared-lock", "shared-lock"));
        let _ = done.send(());
    });
    match finished.recv_timeout(std::time::Duration::from_secs(10)) {
        Ok(()) => {}
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("a start and stop waited on a lock in the state directory itself: {names:?}")
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            panic!("the start and stop failed; its panic is printed above")
        }
    }
    drop(held);
}

/// Every `.json` and `.json.tmp` path under `root`.
fn state_and_temp_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                dirs.push(path);
            } else if path.to_string_lossy().ends_with(".json")
                || path.to_string_lossy().ends_with(".json.tmp")
            {
                found.push(path);
            }
        }
    }
    found
}

/// A write that replaces a state swaps the old state out of its name and unlinks it; it does
/// not rename the new file over the old one. btrfs starts writing a file's data out inside
/// a rename that replaces another file, with the directory locked, so with many clones
/// starting together each such rename waited on the block device behind the VMs' disk
/// reads. The exchange is seen from outside as the old state arriving at the temporary
/// name.
#[tokio::test]
async fn a_write_that_replaces_a_state_starts_no_write_out_in_its_rename() {
    let temp_dir = TempDir::new().unwrap();
    let manager = StateManager::new(temp_dir.path().to_path_buf());
    manager.init().await.unwrap();
    let mut vm = VmState::new("vm-swap".to_string(), "alpine:latest".to_string(), 1, 512);
    vm.name = Some("swap".to_string());
    manager.claim_name(&mut vm).await.unwrap();
    let state_file = state_and_temp_files(temp_dir.path())
        .into_iter()
        .find(|path| path.file_name().is_some_and(|name| name == "vm-swap.json"))
        .expect("the claim wrote the VM's state");

    let inotify = Inotify::init(InitFlags::IN_NONBLOCK).unwrap();
    inotify
        .add_watch(
            state_file.parent().unwrap(),
            AddWatchFlags::IN_MOVED_TO | AddWatchFlags::IN_DELETE,
        )
        .unwrap();
    manager
        .update_health_status("vm-swap", HealthStatus::Healthy, None)
        .await
        .unwrap();

    let mut seen = Vec::new();
    loop {
        match inotify.read_events() {
            Ok(events) if !events.is_empty() => {
                for event in events {
                    let name = event.name.unwrap().to_string_lossy().into_owned();
                    let kind = if event.mask.contains(AddWatchFlags::IN_DELETE) {
                        "deleted"
                    } else {
                        "moved to"
                    };
                    seen.push(format!("{kind} {name}"));
                }
            }
            Ok(_) | Err(nix::errno::Errno::EAGAIN) => break,
            Err(error) => panic!("reading inotify events: {error}"),
        }
    }
    seen.sort();
    assert_eq!(
        seen,
        [
            "deleted vm-swap.json.tmp",
            "moved to vm-swap.json",
            "moved to vm-swap.json.tmp"
        ],
        "the write did not exchange the new state with the old one and unlink the old"
    );
    let published: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state_file).unwrap()).unwrap();
    assert_eq!(published["health_status"], "healthy");
    assert_eq!(
        state_and_temp_files(temp_dir.path()),
        vec![state_file],
        "a write left a temporary file"
    );
    manager.delete_state("vm-swap").await.unwrap();
}
