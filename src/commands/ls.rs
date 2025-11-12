use anyhow::Result;
use tracing::info;

use crate::paths;
use crate::state::StateManager;


pub async fn cmd_ls() -> Result<()> {
    info!("fcvm ls");
    let state_manager = StateManager::new(paths::state_dir());
    let vms = state_manager.list_vms().await?;

    println!("{:<20} {:<10} {:<6} {:<8} {:<20}", "NAME", "STATUS", "CPU", "MEM(MB)", "CREATED");
    println!("{}", "-".repeat(80));

    for vm in vms {
        println!(
            "{:<20} {:<10} {:<6} {:<8} {:<20}",
            vm.name.unwrap_or(vm.vm_id),
            format!("{:?}", vm.status),
            vm.config.vcpu,
            vm.config.memory_mib,
            vm.created_at.format("%Y-%m-%d %H:%M:%S")
        );
    }

    Ok(())
}
