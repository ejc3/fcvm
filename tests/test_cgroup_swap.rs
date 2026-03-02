//! Tests for disable_cgroup_swap: creates a dedicated cgroup under fcvm.slice
//! with memory.swap.max=0 and moves the target process into it.
//!
//! Requires root (cgroup manipulation needs CAP_SYS_ADMIN).

#[cfg(feature = "privileged-tests")]
mod tests {
    use std::process::Command;

    /// Test that disable_cgroup_swap moves a process to /sys/fs/cgroup/fcvm.slice/fcvm-{pid}.scope
    /// with memory.swap.max=0, while not affecting the original cgroup.
    #[test]
    fn test_disable_cgroup_swap_isolates_process() {
        let mut child = Command::new("sleep")
            .arg("300")
            .spawn()
            .expect("failed to spawn sleep");
        let pid = child.id();

        // Read initial cgroup
        let initial_cgroup = std::fs::read_to_string(format!("/proc/{}/cgroup", pid))
            .expect("failed to read cgroup");
        let initial_path = initial_cgroup
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .expect("no cgroup v2 entry")
            .to_string();

        // Disable swap
        fcvm::commands::common::disable_cgroup_swap(pid);

        // Verify process moved to fcvm.slice scope
        let new_cgroup = std::fs::read_to_string(format!("/proc/{}/cgroup", pid))
            .expect("failed to read cgroup after move");
        let new_path = new_cgroup
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .expect("no cgroup v2 entry after move")
            .to_string();

        assert_ne!(initial_path, new_path, "process should have moved cgroups");
        let expected = format!("/fcvm.slice/fcvm-{}.scope", pid);
        assert_eq!(
            new_path, expected,
            "process should be in fcvm.slice/fcvm-{}.scope",
            pid
        );

        // Verify memory.swap.max=0
        let swap_max = std::fs::read_to_string(format!(
            "/sys/fs/cgroup{}/memory.swap.max",
            new_path
        ))
        .expect("failed to read memory.swap.max");
        assert_eq!(swap_max.trim(), "0", "swap should be disabled");

        // Cleanup
        child.kill().expect("failed to kill child");
        child.wait().expect("failed to wait");
        std::thread::sleep(std::time::Duration::from_millis(100));
        let _ = std::fs::remove_dir(format!("/sys/fs/cgroup{}", new_path));
    }

    /// Test that two processes get separate cgroup scopes.
    #[test]
    fn test_disable_cgroup_swap_separate_scopes() {
        let mut child1 = Command::new("sleep")
            .arg("300")
            .spawn()
            .expect("failed to spawn sleep 1");
        let mut child2 = Command::new("sleep")
            .arg("300")
            .spawn()
            .expect("failed to spawn sleep 2");
        let pid1 = child1.id();
        let pid2 = child2.id();

        fcvm::commands::common::disable_cgroup_swap(pid1);
        fcvm::commands::common::disable_cgroup_swap(pid2);

        let cg1 = std::fs::read_to_string(format!("/proc/{}/cgroup", pid1))
            .expect("read cgroup 1");
        let cg2 = std::fs::read_to_string(format!("/proc/{}/cgroup", pid2))
            .expect("read cgroup 2");
        let path1 = cg1.lines().find_map(|l| l.strip_prefix("0::")).unwrap();
        let path2 = cg2.lines().find_map(|l| l.strip_prefix("0::")).unwrap();

        assert_ne!(path1, path2, "each process should get its own scope");
        assert!(path1.contains(&format!("fcvm-{}", pid1)));
        assert!(path2.contains(&format!("fcvm-{}", pid2)));

        // Both should have swap disabled
        for (path, pid) in [(path1, pid1), (path2, pid2)] {
            let swap = std::fs::read_to_string(format!(
                "/sys/fs/cgroup{}/memory.swap.max",
                path
            ))
            .unwrap_or_else(|e| panic!("failed to read swap for pid {}: {}", pid, e));
            assert_eq!(swap.trim(), "0", "swap should be 0 for pid {}", pid);
        }

        // Cleanup
        child1.kill().ok();
        child2.kill().ok();
        child1.wait().ok();
        child2.wait().ok();
        std::thread::sleep(std::time::Duration::from_millis(100));
        let _ = std::fs::remove_dir(format!("/sys/fs/cgroup{}", path1));
        let _ = std::fs::remove_dir(format!("/sys/fs/cgroup{}", path2));
    }
}
