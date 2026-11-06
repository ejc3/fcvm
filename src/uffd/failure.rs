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

/// The VM id of the state whose fcvm process is `(parent, parent_start)`.
pub(crate) fn clone_of_parent(
    states: &[crate::state::VmState],
    parent: u32,
    parent_start: u64,
) -> Option<String> {
    states
        .iter()
        .find(|state| state.pid == Some(parent) && state.pid_start_time == Some(parent_start))
        .map(|state| state.vm_id.clone())
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
        let states = vec![stale, live];
        assert_eq!(clone_of_parent(&states, 100, 2).as_deref(), Some("vm-live"));
        assert_eq!(clone_of_parent(&states, 100, 3), None);
        assert_eq!(clone_of_parent(&states, 101, 2), None);
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
