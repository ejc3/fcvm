use anyhow::{Context, Result};

use crate::cli::args::SetupArgs;
use crate::paths;
use crate::setup::rootfs::{generate_config, get_kernel_profile, load_config};

/// Run setup to download kernel and create rootfs.
///
/// This downloads the Kata kernel (~15MB) and creates the Layer 2 rootfs (~10GB).
/// The rootfs creation downloads Ubuntu cloud image and installs podman, taking 5-10 minutes.
pub async fn cmd_setup(args: SetupArgs) -> Result<()> {
    // Handle --generate-config: write default config and exit
    if args.generate_config {
        let config_path = generate_config(args.force)?;
        println!("Generated config at: {}", config_path.display());
        println!("\nCustomize the config file, then run:");
        println!("  sudo fcvm setup");
        return Ok(());
    }

    // For host kernel install only, use temp paths (no btrfs needed)
    if args.install_host_kernel {
        if args.kernel_profile.is_none() {
            anyhow::bail!("--install-host-kernel requires --kernel-profile");
        }
        let profile_name = args.kernel_profile.as_ref().unwrap();

        // Use /tmp for kernel build (no btrfs required)
        paths::init_with_paths("/tmp/fcvm-kernel", "/tmp/fcvm-kernel");
        std::fs::create_dir_all("/tmp/fcvm-kernel/kernels")?;

        let profile = get_kernel_profile(profile_name)?.ok_or_else(|| {
            anyhow::anyhow!("kernel profile '{}' not found in config", profile_name)
        })?;

        println!(
            "Building and installing host kernel with profile '{}'...",
            profile_name
        );

        // Build the profile kernel
        let profile_kernel_path =
            crate::setup::ensure_kernel(Some(profile_name), true, args.build_kernels)
                .await
                .context("building profile kernel")?;
        println!("  ✓ Kernel built: {}", profile_kernel_path.display());

        // Install as host kernel
        println!("\nInstalling host kernel with fcvm patches...");
        crate::setup::install_host_kernel(&profile, profile.boot_args.as_deref())
            .await
            .context("installing host kernel")?;

        return Ok(());
    }

    // Ensure btrfs storage is ready (creates loopback if needed)
    // This must be done before accessing any paths under the configured assets_dir
    crate::setup::ensure_storage(args.config.as_deref()).context("initializing storage")?;

    // Load config and initialize paths (with helpful error if config missing)
    let (config, _, _) = load_config(args.config.as_deref())?;
    paths::init_with_paths(&config.paths.data_dir, &config.paths.assets_dir);

    println!("Setting up fcvm (this may take 5-10 minutes on first run)...");

    // Ensure default kernel exists (downloads from [kernel] section if missing)
    let kernel_path = crate::setup::ensure_kernel(None, true, false)
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

    // Setup kernel profile if requested (e.g., "nested" for nested virtualization)
    if let Some(profile_name) = &args.kernel_profile {
        let profile = get_kernel_profile(profile_name)?.ok_or_else(|| {
            anyhow::anyhow!("kernel profile '{}' not found in config", profile_name)
        })?;

        println!(
            "\nSetting up kernel profile '{}': {}",
            profile_name, profile.description
        );

        // Download or build the profile kernel
        let profile_kernel_path =
            crate::setup::ensure_kernel(Some(profile_name), true, args.build_kernels)
                .await
                .context("setting up profile kernel")?;
        println!(
            "  ✓ Profile kernel ready: {}",
            profile_kernel_path.display()
        );

        // Build profile firecracker if needed
        crate::setup::ensure_profile_firecracker(&profile, profile_name)
            .await
            .context("setting up profile firecracker")?;

        println!("\nFor '{}' profile, use:", profile_name);
        println!(
            "  fcvm podman run --kernel-profile {} --privileged ...",
            profile_name
        );
    }

    println!("\nSetup complete! You can now run VMs with: fcvm podman run ...");

    Ok(())
}
