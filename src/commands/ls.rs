use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::cli::LsArgs;
use crate::paths;
use crate::state::{truncate_id, StateManager, VmState};

const STALE_THRESHOLD_SECS: i64 = 300; // 5 minutes

/// Extended VM info for display with computed fields
#[derive(Debug, Serialize, Deserialize)]
struct VmInfoDisplay {
    #[serde(flatten)]
    vm: VmState,
    stale: bool,
}

pub async fn cmd_ls(args: LsArgs) -> Result<()> {
    // Only log in non-JSON mode to avoid mixing logs with JSON output
    if !args.json {
        info!("fcvm ls");
    }
    let state_manager = StateManager::new(paths::state_dir());
    let mut vms = state_manager.list_vms().await?;

    let mut vm_displays = Vec::new();

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

        vm_displays.push(VmInfoDisplay {
            vm: vm.clone(),
            stale,
        });
    }

    if args.json {
        // JSON output - serializes VmState with all typed fields
        let json = serde_json::to_string_pretty(&vm_displays)?;
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

        for display in vm_displays {
            let vm = &display.vm;
            let stale_marker = if display.stale { "YES" } else { "" };
            let pid_str = vm.pid.map_or("-".to_string(), |p| p.to_string());
            let name = vm
                .name
                .as_ref()
                .map(|s| s.as_str())
                .unwrap_or_else(|| truncate_id(&vm.vm_id, 8));
            let guest_ip = vm.config.network.guest_ip.as_deref().unwrap_or("-");
            let tap_device = if vm.config.network.tap_device.is_empty() {
                "-"
            } else {
                &vm.config.network.tap_device
            };
            let image = vm
                .config
                .image
                .split(':')
                .next()
                .unwrap_or(&vm.config.image);
            let status = format!("{:?}", vm.status);
            let health = format!("{:?}", vm.health_status);

            println!(
                "{:<20} {:<10} {:<12} {:<12} {:<15} {:<15} {:<12} {:<8} {:<6}",
                name,
                pid_str,
                status,
                health,
                guest_ip,
                tap_device,
                image,
                vm.config.memory_mib,
                stale_marker
            );
        }
    }

    Ok(())
}
