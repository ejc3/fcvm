use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::cli::LsArgs;
use crate::paths;
use crate::state::{truncate_id, StateManager};

const STALE_THRESHOLD_SECS: i64 = 300; // 5 minutes

#[derive(Debug, Serialize, Deserialize)]
struct VmInfo {
    name: String,
    status: String,
    health: String,
    guest_ip: String,
    tap_device: String,
    image: String,
    mem_mb: u32,
    pid: Option<u32>,
    stale: bool,
    last_updated: String,
}

pub async fn cmd_ls(args: LsArgs) -> Result<()> {
    // Only log in non-JSON mode to avoid mixing logs with JSON output
    if !args.json {
        info!("fcvm ls");
    }
    let state_manager = StateManager::new(paths::state_dir());
    let mut vms = state_manager.list_vms().await?;

    let mut vm_infos = Vec::new();

    for vm in &mut vms {
        // Filter by PID if requested
        if let Some(filter_pid) = args.pid {
            if vm.pid != Some(filter_pid) {
                continue;
            }
        }

        // Check if state is stale (no update in 5 minutes)
        let now = Utc::now();
        let elapsed = now.signed_duration_since(vm.last_updated);
        let stale = elapsed.num_seconds() > STALE_THRESHOLD_SECS;

        // Verify PID is actually running by checking /proc/{pid}
        if let Some(pid) = vm.pid {
            let proc_path = format!("/proc/{}", pid);
            if !std::path::Path::new(&proc_path).exists() {
                // Process no longer exists, mark as stopped
                vm.status = crate::state::VmStatus::Stopped;
                vm.health_status = crate::state::HealthStatus::Unknown;
            }
        }

        let name = vm.name.clone().unwrap_or_else(|| truncate_id(&vm.vm_id, 8).to_string());

        // Extract network info from config
        let (guest_ip, tap_device) = if let Some(network) = vm.config.network.as_object() {
            let ip = network
                .get("guest_ip")
                .and_then(|v| v.as_str())
                .unwrap_or("-");
            let tap = network
                .get("tap_device")
                .and_then(|v| v.as_str())
                .unwrap_or("-");
            (ip.to_string(), tap.to_string())
        } else {
            ("-".to_string(), "-".to_string())
        };

        let status = format!("{:?}", vm.status);
        let health = format!("{:?}", vm.health_status);
        let image = vm
            .config
            .image
            .split(':')
            .next()
            .unwrap_or(&vm.config.image);

        vm_infos.push(VmInfo {
            name,
            status,
            health,
            guest_ip,
            tap_device,
            image: image.to_string(),
            mem_mb: vm.config.memory_mib,
            pid: vm.pid,
            stale,
            last_updated: vm.last_updated.to_rfc3339(),
        });
    }

    if args.json {
        // JSON output
        let json = serde_json::to_string_pretty(&vm_infos)?;
        println!("{}", json);
    } else {
        // Table output
        println!(
            "{:<20} {:<10} {:<12} {:<12} {:<15} {:<15} {:<12} {:<8} {:<6}",
            "NAME",
            "PID",
            "STATUS",
            "HEALTH",
            "GUEST_IP",
            "TAP_DEVICE",
            "IMAGE",
            "MEM(MB)",
            "STALE"
        );
        println!("{}", "-".repeat(120));

        for vm in vm_infos {
            let stale_marker = if vm.stale { "YES" } else { "" };
            let pid_str = vm.pid.map_or("-".to_string(), |p| p.to_string());

            println!(
                "{:<20} {:<10} {:<12} {:<12} {:<15} {:<15} {:<12} {:<8} {:<6}",
                vm.name,
                pid_str,
                vm.status,
                vm.health,
                vm.guest_ip,
                vm.tap_device,
                vm.image,
                vm.mem_mb,
                stale_marker
            );
        }
    }

    Ok(())
}
