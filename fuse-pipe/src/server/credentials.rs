//! Unix credentials handling for proper permission enforcement.
//!
//! This module provides a RAII guard for temporarily switching filesystem
//! uid/gid when performing filesystem operations. This ensures that operations
//! are performed with the correct permissions as the requesting user.
//!
//! Uses setfsuid()/setfsgid() syscalls which control which user/group is used
//! for filesystem access checks. These are per-thread and don't affect other
//! credentials (euid/egid remain unchanged).
//!
//! Based on the approach used by virtiofsd and NFS servers.

use std::io;

/// Guard that temporarily switches filesystem uid/gid and restores them on drop.
///
/// This uses setfsuid/setfsgid syscalls which ONLY affect filesystem access
/// checks for the calling thread. The euid/egid remain unchanged, so the
/// process can still perform privileged operations.
///
/// # Thread Safety
///
/// Each thread maintains its own fsuid/fsgid via syscalls.
/// This is safe for concurrent access from multiple threads.
pub struct CredentialsGuard {
    original_fsuid: u32,
    original_fsgid: u32,
    active: bool,
}

impl CredentialsGuard {
    /// Create a new credentials guard that switches filesystem uid/gid.
    ///
    /// If uid is 0 (root), no switching is performed since root already has
    /// full access.
    ///
    /// # Errors
    ///
    /// Returns an error if the privilege switch fails.
    pub fn new(uid: u32, gid: u32) -> io::Result<Self> {
        // Get current fsuid/fsgid by calling with -1 (which doesn't change them)
        // setfsuid/setfsgid return i32 but take u32, cast appropriately
        let original_fsuid = unsafe { libc::setfsuid(u32::MAX) } as u32;
        let original_fsgid = unsafe { libc::setfsgid(u32::MAX) } as u32;

        // If caller is root or we're already the target user, no switch needed
        if uid == 0 || (uid == original_fsuid && gid == original_fsgid) {
            return Ok(Self {
                original_fsuid,
                original_fsgid,
                active: false,
            });
        }

        // Switch fsgid first (must do this before dropping fsuid privileges)
        // setfsgid returns the previous fsgid, not an error code
        let prev_gid = unsafe { libc::setfsgid(gid) } as u32;
        // Verify the change took effect by reading back
        let new_gid = unsafe { libc::setfsgid(gid) } as u32;
        if new_gid != gid {
            // Failed to set gid, restore original
            unsafe { libc::setfsgid(prev_gid) };
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }

        // Switch fsuid
        let prev_uid = unsafe { libc::setfsuid(uid) } as u32;
        // Verify the change took effect
        let new_uid = unsafe { libc::setfsuid(uid) } as u32;
        if new_uid != uid {
            // Failed to set uid, restore originals
            unsafe {
                libc::setfsuid(prev_uid);
                libc::setfsgid(original_fsgid);
            }
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }

        Ok(Self {
            original_fsuid,
            original_fsgid,
            active: true,
        })
    }

    /// Create a guard that does nothing (for operations that don't need credential switching).
    pub fn noop() -> Self {
        let original_fsuid = unsafe { libc::setfsuid(u32::MAX) } as u32;
        let original_fsgid = unsafe { libc::setfsgid(u32::MAX) } as u32;
        Self {
            original_fsuid,
            original_fsgid,
            active: false,
        }
    }
}

impl Drop for CredentialsGuard {
    fn drop(&mut self) {
        if self.active {
            // Restore original filesystem credentials
            unsafe {
                libc::setfsuid(self.original_fsuid);
                libc::setfsgid(self.original_fsgid);
            }
        }
    }
}

/// Helper macro for running an operation with credentials.
///
/// This creates a credentials guard, runs the operation, and returns the result.
/// The guard is automatically dropped after the operation completes.
#[macro_export]
macro_rules! with_credentials {
    ($uid:expr, $gid:expr, $op:expr) => {{
        let _guard = $crate::server::credentials::CredentialsGuard::new($uid, $gid)?;
        $op
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_noop_guard() {
        let guard = CredentialsGuard::noop();
        assert!(!guard.active);
    }

    #[test]
    fn test_root_noop() {
        // If we're root, switching to root should be a noop
        let original_uid = unsafe { libc::geteuid() };
        if original_uid == 0 {
            let guard = CredentialsGuard::new(0, 0).unwrap();
            assert!(!guard.active);
        }
    }

    #[test]
    fn test_same_user_noop() {
        // Get current fsuid/fsgid (setfsuid/setfsgid with -1 returns current value)
        let original_fsuid = unsafe { libc::setfsuid(u32::MAX) } as u32;
        let original_fsgid = unsafe { libc::setfsgid(u32::MAX) } as u32;
        let guard = CredentialsGuard::new(original_fsuid, original_fsgid).unwrap();
        assert!(!guard.active);
    }
}
