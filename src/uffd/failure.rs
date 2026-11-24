//! A memory server's fail-closed kill, recorded where the killed clone reads it.
//!
//! A memory server that can no longer serve a clone's page faults SIGKILLs the clone's VMM
//! (`serve_clone_fail_closed` in `server.rs`). The clone's `fcvm snapshot run` sees only a
//! VMM that died of SIGKILL, and its own power-off watchdog sends the same signal to a guest
//! that would not power off after its container exited. When both kills land in the same
//! window, the clone could not tell them apart and reported the server's kill as its own,
//! exiting 0 for a clone whose memory was never served (#1007).
//!
//! So the server writes the reason into the clone's runtime directory before it sends the
//! signal, and the clone reads that file after it has reaped its VMM. The write comes first,
//! so a clone that reaps a VMM the server killed always finds it.
//!
//! The server knows the VMM only by the pidfd its connection handed over. The VMM's parent
//! is the clone's fcvm process, whose state names the clone's runtime directory: the server
//! reads the parent from `/proc/<vmm>/stat` (never from the VMM's memory), checks the pidfd
//! still pins a live VMM after that read so the PID was not reused under it, and picks the
//! state whose `(pid, pid_start_time)` is that parent.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The file in a clone's runtime directory that holds the reason its memory server killed
/// its VMM.
pub const FAILURE_FILE: &str = "uffd-failure";

/// The parent PID of `pid`, from field 4 of `/proc/<pid>/stat`.
pub(crate) fn parent_pid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Everything after the last ')' is space separated, starting at field 3 (state).
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// The VM id of the state whose fcvm process is `(parent, parent_start)`. None when no state
/// is, and when more than one is: a process running several VMs (a library caller) does not
/// say which of them owns this VMM, and recording the kill in the wrong one would make that
/// clone report a failure it did not have.
pub(crate) fn clone_of_parent(
    states: &[crate::state::VmState],
    parent: u32,
    parent_start: u64,
) -> Option<String> {
    let mut owners = states
        .iter()
        .filter(|state| state.pid == Some(parent) && state.pid_start_time == Some(parent_start));
    let owner = owners.next()?;
    match owners.next() {
        Some(_) => None,
        None => Some(owner.vm_id.clone()),
    }
}

/// The states among which the clone of fcvm process `(parent, parent_start)` is looked for,
/// and whether every VM's state was read to get them.
///
/// The PID index names the states saved by a process, so a clone is found by reading its own
/// state: a memory server refusing many clones at once must not read every VM's state once
/// per refusal on the runtime that serves page faults. All states are listed only when the
/// index names no state of that process, which is a state `save_state` could not enter in
/// the index (it logs that) or one written without it.
pub(crate) async fn states_of_parent(
    manager: &crate::state::StateManager,
    parent: u32,
    parent_start: u64,
) -> Result<(Vec<crate::state::VmState>, bool)> {
    let indexed = manager.indexed_states_of_pid(parent).await;
    if indexed
        .iter()
        .any(|state| state.pid_start_time == Some(parent_start))
    {
        return Ok((indexed, false));
    }
    let listed = manager.list_vms().await.context("listing VM states")?;
    Ok((listed, true))
}

/// Write `reason` to `dir`'s failure file, atomically.
pub(crate) fn write_failure(dir: &Path, reason: &str) -> Result<PathBuf> {
    let path = dir.join(FAILURE_FILE);
    let tmp = dir.join(format!(".{FAILURE_FILE}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, reason).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("renaming to {}", path.display()))?;
    Ok(path)
}

/// The reason a memory server recorded for killing the VMM of the clone whose runtime
/// directory is `dir`, if one did.
pub fn read_failure(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join(FAILURE_FILE)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_parent_pid_is_read_from_stat() {
        assert_eq!(
            parent_pid(std::process::id()),
            Some(std::os::unix::process::parent_id())
        );
    }

    /// The server finds the clone by its fcvm process's identity, PID and start time, so a
    /// stale state that only shares the PID is not it.
    #[test]
    fn the_clone_is_the_state_of_the_vmm_s_parent() {
        let mut stale = crate::state::VmState::new("vm-stale".into(), "alpine".into(), 1, 128);
        stale.pid = Some(100);
        stale.pid_start_time = Some(1);
        let mut live = crate::state::VmState::new("vm-live".into(), "alpine".into(), 1, 128);
        live.pid = Some(100);
        live.pid_start_time = Some(2);
        let mut states = vec![stale, live];
        assert_eq!(clone_of_parent(&states, 100, 2).as_deref(), Some("vm-live"));
        assert_eq!(clone_of_parent(&states, 100, 3), None);
        assert_eq!(clone_of_parent(&states, 101, 2), None);
        // Two VMs of one process: the VMM's owner is not known, so neither is chosen.
        let mut sibling = crate::state::VmState::new("vm-sibling".into(), "alpine".into(), 1, 128);
        sibling.pid = Some(100);
        sibling.pid_start_time = Some(2);
        states.push(sibling);
        assert_eq!(clone_of_parent(&states, 100, 2), None);
    }

    /// A clone whose state went through `save_state` is found through the PID index, without
    /// listing every VM's state; a state the index does not name is still found, by listing.
    #[tokio::test]
    async fn the_clone_is_found_through_the_pid_index_without_listing() {
        let dir = tempfile::tempdir().unwrap();
        let manager = crate::state::StateManager::new(dir.path().to_path_buf());
        manager.init().await.unwrap();
        let pid = std::process::id();
        let start = crate::utils::process_start_time(pid).unwrap();
        let mut clone = crate::state::VmState::new("vm-clone".into(), "alpine".into(), 1, 128);
        clone.pid = Some(pid);
        manager.save_state(&clone).await.unwrap();

        let (states, listed) = states_of_parent(&manager, pid, start).await.unwrap();
        assert!(
            !listed,
            "every VM's state was read to find an indexed clone"
        );
        assert_eq!(
            clone_of_parent(&states, pid, start).as_deref(),
            Some("vm-clone")
        );

        // A state the index does not name: written where states go, with no index entry.
        let parent = std::os::unix::process::parent_id();
        let parent_start = crate::utils::process_start_time(parent).unwrap();
        let mut unindexed =
            crate::state::VmState::new("vm-unindexed".into(), "alpine".into(), 1, 128);
        unindexed.pid = Some(parent);
        unindexed.pid_start_time = Some(parent_start);
        let unindexed_path = manager.state_file("vm-unindexed");
        std::fs::create_dir_all(unindexed_path.parent().unwrap()).unwrap();
        std::fs::write(unindexed_path, serde_json::to_string(&unindexed).unwrap()).unwrap();
        let (states, listed) = states_of_parent(&manager, parent, parent_start)
            .await
            .unwrap();
        assert!(listed);
        assert_eq!(
            clone_of_parent(&states, parent, parent_start).as_deref(),
            Some("vm-unindexed")
        );
    }

    #[test]
    fn a_written_failure_is_read_back() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_failure(dir.path()), None);
        write_failure(dir.path(), "page fault service failed").unwrap();
        assert_eq!(
            read_failure(dir.path()).as_deref(),
            Some("page fault service failed")
        );
    }
}
