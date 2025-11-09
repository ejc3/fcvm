use anyhow::Result;

use crate::cli::{SetupArgs, SetupCommands};
use crate::setup;

pub async fn cmd_setup(args: SetupArgs) -> Result<()> {
    match args.cmd {
        SetupCommands::Kernel { output, download } => {
            setup::kernel::setup_kernel(&output, download).await
        }
        SetupCommands::Rootfs { output, suite, size_mb } => {
            setup::rootfs::setup_rootfs(&output, &suite, size_mb).await
        }
        SetupCommands::Preflight => {
            setup::preflight::check_preflight().await
        }
    }
}
