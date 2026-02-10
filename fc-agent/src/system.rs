use tokio::{
    process::Command,
    time::{sleep, Duration},
};

use crate::types::Plan;

pub const PROXY_SETTINGS_FILE: &str = "/etc/fcvm-proxy.env";

/// Ensure cgroup v2 pids controller is available for container creation.
pub async fn wait_for_cgroup_controllers() {
    use tokio::fs;

    const REQUIRED_CONTROLLER: &str = "pids";

    let my_cgroup = match fs::read_to_string("/proc/self/cgroup").await {
        Ok(content) => content
            .lines()
            .find(|l| l.starts_with("0::"))
            .map(|l| l.strip_prefix("0::").unwrap_or("/").to_string())
            .unwrap_or_else(|| "/".to_string()),
        Err(e) => {
            eprintln!(
                "[fc-agent] WARNING: failed to read /proc/self/cgroup: {}",
                e
            );
            "/".to_string()
        }
    };

    eprintln!("[fc-agent] current cgroup: {}", my_cgroup);

    let mut paths_to_enable = vec!["/sys/fs/cgroup".to_string()];
    let mut current_path = "/sys/fs/cgroup".to_string();

    for component in my_cgroup.trim_start_matches('/').split('/') {
        if component.is_empty() {
            continue;
        }
        current_path = format!("{}/{}", current_path, component);
        paths_to_enable.push(current_path.clone());
    }

    eprintln!(
        "[fc-agent] enabling pids controller in cgroup chain: {:?}",
        paths_to_enable
    );

    for cgroup_path in &paths_to_enable {
        let subtree_control_path = format!("{}/cgroup.subtree_control", cgroup_path);

        match fs::read_to_string(&subtree_control_path).await {
            Ok(controllers) => {
                let available: Vec<&str> = controllers.split_whitespace().collect();
                if available.contains(&REQUIRED_CONTROLLER) {
                    continue;
                }

                match fs::write(&subtree_control_path, format!("+{}\n", REQUIRED_CONTROLLER)).await
                {
                    Ok(()) => {
                        eprintln!(
                            "[fc-agent] enabled '{}' controller in {}",
                            REQUIRED_CONTROLLER, subtree_control_path
                        );
                    }
                    Err(e) => {
                        eprintln!(
                            "[fc-agent] WARNING: failed to enable '{}' in {}: {}",
                            REQUIRED_CONTROLLER, subtree_control_path, e
                        );
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "[fc-agent] WARNING: failed to read {}: {}",
                    subtree_control_path, e
                );
            }
        }
    }

    // Verify pids is now available in our parent's subtree_control
    let parent_subtree = if my_cgroup == "/" {
        "/sys/fs/cgroup/cgroup.subtree_control".to_string()
    } else {
        let parent_cgroup = std::path::Path::new(&my_cgroup)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "/".to_string());
        if parent_cgroup == "/" || parent_cgroup.is_empty() {
            "/sys/fs/cgroup/cgroup.subtree_control".to_string()
        } else {
            format!("/sys/fs/cgroup{}/cgroup.subtree_control", parent_cgroup)
        }
    };

    match fs::read_to_string(&parent_subtree).await {
        Ok(controllers) => {
            let available: Vec<&str> = controllers.split_whitespace().collect();
            if available.contains(&REQUIRED_CONTROLLER) {
                eprintln!(
                    "[fc-agent] cgroup controllers available in {}: {}",
                    parent_subtree,
                    controllers.trim()
                );
            } else {
                eprintln!(
                    "[fc-agent] WARNING: '{}' not available in {} after enabling (available: {})",
                    REQUIRED_CONTROLLER,
                    parent_subtree,
                    controllers.trim()
                );
            }
        }
        Err(e) => {
            eprintln!(
                "[fc-agent] WARNING: failed to verify controllers in {}: {}",
                parent_subtree, e
            );
        }
    }
}

/// Create /dev/kvm for nested virtualization (no-op if kernel lacks CONFIG_KVM).
pub fn create_kvm_device() {
    use std::path::Path;

    let kvm_path = Path::new("/dev/kvm");
    if kvm_path.exists() {
        eprintln!("[fc-agent] /dev/kvm already exists");
        return;
    }

    let dev = libc::makedev(10, 232);
    let result = unsafe { libc::mknod(c"/dev/kvm".as_ptr(), libc::S_IFCHR | 0o666, dev) };

    if result == 0 {
        eprintln!("[fc-agent] created /dev/kvm (10:232)");
    } else {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::NotFound || err.raw_os_error() == Some(libc::ENOENT) {
            eprintln!("[fc-agent] /dev/kvm not available (kernel needs CONFIG_KVM)");
        } else {
            eprintln!("[fc-agent] WARNING: failed to create /dev/kvm: {}", err);
        }
    }
}

/// Raise RLIMIT_NOFILE to 65536.
pub fn raise_resource_limits() {
    let new_limit = libc::rlimit {
        rlim_cur: 65536,
        rlim_max: 65536,
    };

    let result = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &new_limit) };
    if result == 0 {
        eprintln!("[fc-agent] raised RLIMIT_NOFILE to 65536");
    } else {
        eprintln!(
            "[fc-agent] WARNING: failed to raise RLIMIT_NOFILE: {}",
            std::io::Error::last_os_error()
        );
    }
}

/// Shutdown the VM. This function never returns.
pub async fn shutdown_vm(exit_code: i32) -> ! {
    eprintln!("[fc-agent] shutting down VM (exit_code={})", exit_code);

    if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
        let fuse_mounts: Vec<&str> = mounts.lines().filter(|l| l.contains("fuse")).collect();
        if !fuse_mounts.is_empty() {
            eprintln!("[fc-agent] FUSE mounts before shutdown: {:?}", fuse_mounts);
        }
    }

    eprintln!("[fc-agent] starting sync...");
    let sync_start = std::time::Instant::now();
    if let Ok(mut sync_child) = Command::new("sync").spawn() {
        for _ in 0..20 {
            match sync_child.try_wait() {
                Ok(Some(status)) => {
                    eprintln!(
                        "[fc-agent] sync completed in {:?} with status: {:?}",
                        sync_start.elapsed(),
                        status
                    );
                    break;
                }
                Ok(None) => {
                    sleep(Duration::from_millis(100)).await;
                }
                Err(e) => {
                    eprintln!("[fc-agent] sync wait error: {}", e);
                    break;
                }
            }
        }
        if sync_start.elapsed().as_secs() >= 2 {
            eprintln!("[fc-agent] sync timed out after 2s, killing it");
            let _ = sync_child.kill().await;
        }
    }

    #[cfg(target_arch = "aarch64")]
    {
        eprintln!("[fc-agent] calling poweroff -f (PSCI SYSTEM_OFF)...");
        let _ = Command::new("poweroff").args(["-f"]).spawn();
    }
    #[cfg(target_arch = "x86_64")]
    {
        eprintln!("[fc-agent] calling reboot -f (triple-fault via reboot=t)...");
        let _ = Command::new("reboot").args(["-f"]).spawn();
    }

    sleep(Duration::from_secs(2)).await;

    #[cfg(target_arch = "aarch64")]
    {
        eprintln!("[fc-agent] poweroff didn't complete after 2s, trying reboot -f");
        let _ = Command::new("reboot").args(["-f"]).spawn();
    }
    #[cfg(target_arch = "x86_64")]
    {
        eprintln!("[fc-agent] reboot didn't complete after 2s, trying sysrq");
    }

    sleep(Duration::from_secs(2)).await;
    eprintln!("[fc-agent] shutdown didn't complete, trying sysrq reboot");
    let _ = std::fs::write("/proc/sysrq-trigger", "b");

    sleep(Duration::from_secs(1)).await;
    eprintln!("[fc-agent] VM shutdown completely failed!");
    std::process::exit(exit_code)
}

/// Extract clone ID from hostname or PID.
pub async fn get_clone_id() -> String {
    if let Ok(output) = Command::new("hostname").output().await {
        let hostname = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if hostname.starts_with("clone-lock-") {
            if let Some(id) = hostname.strip_prefix("clone-lock-") {
                return id.to_string();
            }
        }
        if hostname.chars().all(|c| c.is_ascii_digit()) {
            return hostname;
        }
    }
    std::process::id().to_string()
}

/// Save proxy settings from plan to file and environment.
pub fn save_proxy_settings(plan: &Plan) {
    use std::io::Write as _;

    let mut content = String::new();
    let mut env_vars = Vec::new();

    if let Some(ref proxy) = plan.http_proxy {
        content.push_str(&format!("http_proxy={}\n", proxy));
        content.push_str(&format!("HTTP_PROXY={}\n", proxy));
        env_vars.push(("http_proxy", proxy.clone()));
        env_vars.push(("HTTP_PROXY", proxy.clone()));
    }
    if let Some(ref proxy) = plan.https_proxy {
        content.push_str(&format!("https_proxy={}\n", proxy));
        content.push_str(&format!("HTTPS_PROXY={}\n", proxy));
        env_vars.push(("https_proxy", proxy.clone()));
        env_vars.push(("HTTPS_PROXY", proxy.clone()));
    }
    if let Some(ref no_proxy) = plan.no_proxy {
        content.push_str(&format!("no_proxy={}\n", no_proxy));
        content.push_str(&format!("NO_PROXY={}\n", no_proxy));
        env_vars.push(("no_proxy", no_proxy.clone()));
        env_vars.push(("NO_PROXY", no_proxy.clone()));
    }

    if content.is_empty() {
        eprintln!("[fc-agent] no proxy settings configured");
        return;
    }

    for (key, value) in &env_vars {
        std::env::set_var(key, value);
    }
    eprintln!(
        "[fc-agent] set {} proxy environment variables",
        env_vars.len()
    );

    match std::fs::File::create(PROXY_SETTINGS_FILE) {
        Ok(mut file) => {
            if let Err(e) = file.write_all(content.as_bytes()) {
                eprintln!("[fc-agent] WARNING: failed to write proxy settings: {}", e);
            } else {
                eprintln!("[fc-agent] saved proxy settings to {}", PROXY_SETTINGS_FILE);
            }
        }
        Err(e) => {
            eprintln!(
                "[fc-agent] WARNING: failed to create proxy settings file: {}",
                e
            );
        }
    }
}

/// Read proxy settings from the saved file.
pub fn read_proxy_settings() -> Vec<(String, String)> {
    let content = match std::fs::read_to_string(PROXY_SETTINGS_FILE) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    content
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}
