use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::cli::LsArgs;
use crate::firecracker::FcNetworkMode;
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

/// HOST_ADDR: the host address a published port answers on when its mapping names none.
///
/// That is the VM's loopback IP in rootless and routed mode, and the veth's host address
/// in bridged mode. A routed VM's `host_ip` is the gateway inside its namespace, which
/// nothing on the host reaches, so a routed VM without a loopback IP shows none.
fn host_addr(vm: &VmState) -> &str {
    let network = &vm.config.network;
    let veth_address = match vm.config.network_mode {
        FcNetworkMode::Bridged => network.host_ip.as_deref(),
        FcNetworkMode::Rootless | FcNetworkMode::Routed => None,
    };
    network
        .loopback_ip
        .as_deref()
        .or(veth_address)
        .unwrap_or("-")
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
            "NAME", "PID", "STATUS", "HEALTH", "HOST_ADDR", "GUEST_IP", "IMAGE", "MEM(MB)", "STALE"
        );
        println!("{}", "-".repeat(120));

        for display in vm_displays {
            let vm = &display.vm;
            let stale_marker = if display.stale { "YES" } else { "" };
            let pid_str = vm.pid.map_or("-".to_string(), |p| p.to_string());
            let name = vm
                .name
                .as_deref()
                .unwrap_or_else(|| truncate_id(&vm.vm_id, 8));
            let host_addr = host_addr(vm);
            let guest_ip = vm.config.network.guest_ip.as_deref().unwrap_or("-");
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
                host_addr,
                guest_ip,
                image,
                vm.config.memory_mib,
                stale_marker
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm(mode: FcNetworkMode, loopback_ip: Option<&str>, host_ip: Option<&str>) -> VmState {
        let mut vm = VmState::new("vm-ls".to_string(), "alpine".to_string(), 1, 128);
        vm.config.network_mode = mode;
        vm.config.network.loopback_ip = loopback_ip.map(str::to_string);
        vm.config.network.host_ip = host_ip.map(str::to_string);
        vm
    }

    /// HOST_ADDR is an address the host can reach. A routed VM whose mappings all name
    /// their own address has no loopback IP, and its `host_ip` is the gateway inside its
    /// namespace, so it shows none.
    #[test]
    fn host_addr_is_an_address_on_the_host() {
        use FcNetworkMode::{Bridged, Rootless, Routed};
        let gateway = Some("10.0.2.2");
        assert_eq!(
            host_addr(&vm(Rootless, Some("127.0.0.2"), gateway)),
            "127.0.0.2"
        );
        assert_eq!(
            host_addr(&vm(Routed, Some("127.0.0.3"), gateway)),
            "127.0.0.3"
        );
        assert_eq!(host_addr(&vm(Routed, None, gateway)), "-");
        assert_eq!(
            host_addr(&vm(Bridged, None, Some("172.30.0.1"))),
            "172.30.0.1"
        );
        assert_eq!(host_addr(&vm(Bridged, None, None)), "-");
    }
}
