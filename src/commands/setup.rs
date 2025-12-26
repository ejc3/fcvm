use anyhow::{Context, Result};

/// Run setup to download kernel and create rootfs.
///
/// This downloads the Kata kernel (~15MB) and creates the Layer 2 rootfs (~10GB).
/// The rootfs creation downloads Ubuntu cloud image and installs podman, taking 5-10 minutes.
pub async fn cmd_setup() -> Result<()> {
    println!("Setting up fcvm (this may take 5-10 minutes on first run)...");

    // Ensure kernel exists (downloads Kata kernel if missing)
    let kernel_path = crate::setup::ensure_kernel(true)
        .await
        .context("setting up kernel")?;
    println!("  ✓ Kernel ready: {}", kernel_path.display());

    // Ensure rootfs exists (creates Layer 2 if missing)
    let rootfs_path = crate::setup::ensure_rootfs(true)
        .await
        .context("setting up rootfs")?;
    println!("  ✓ Rootfs ready: {}", rootfs_path.display());

    // Ensure fc-agent initrd exists
    let initrd_path = crate::setup::ensure_fc_agent_initrd(true)
        .await
        .context("setting up fc-agent initrd")?;
    println!("  ✓ Initrd ready: {}", initrd_path.display());

    println!("\nSetup complete! You can now run VMs with: fcvm podman run ...");

    Ok(())
}
