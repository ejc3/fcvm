//! Utility functions for process management and system operations.

use std::path::Path;

/// Check if a process is alive by checking /proc/{pid} existence.
///
/// This is more reliable than sending signal 0 because it doesn't require
/// any special permissions - any user can check if /proc/{pid} exists.
///
/// # Arguments
/// * `pid` - Process ID to check
///
/// # Returns
/// `true` if the process exists, `false` otherwise
pub fn is_process_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{}", pid)).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_process_alive_current_process() {
        // Current process should always be alive
        assert!(is_process_alive(std::process::id()));
    }

    #[test]
    fn test_is_process_alive_nonexistent() {
        // PID 4294967295 (u32::MAX) is extremely unlikely to exist
        assert!(!is_process_alive(u32::MAX));
    }

    #[test]
    fn test_is_process_alive_init() {
        // PID 1 (init/systemd) should always exist on Linux
        assert!(is_process_alive(1));
    }
}
