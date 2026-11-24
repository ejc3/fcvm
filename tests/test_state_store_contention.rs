//! A VM's start and stop write into no directory that every VM shares and wait on no lock
//! that every VM takes.
//!
//! With every state file in the state directory itself and a host-wide lock around each
//! name claim, 128 clones starting together each waited 0.3 to 0.5 s for that lock and 0.5
//! to 0.8 s for the directory's own lock, which a create or a rename holds exclusively
//! (bench/start-rate). These tests use only what a launch calls, so they fail on a layout
//! that puts either back.

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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        start_and_stop(&manager, "vm-shared-lock", "shared-lock"),
    )
    .await
    .unwrap_or_else(|_| {
        panic!("a start and stop waited on a lock in the state directory itself: {names:?}")
    });
    drop(held);
}
