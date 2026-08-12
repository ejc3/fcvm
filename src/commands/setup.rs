use anyhow::{Context, Result};

use crate::cli::args::SetupArgs;
use crate::paths;
use crate::setup::rootfs::{generate_config, get_kernel_profile, load_config};

/// Run setup to download kernel and create rootfs.
///
/// This downloads the released default kernel and creates the Layer 2 rootfs (~10GB).
/// The rootfs creation downloads Ubuntu cloud image and installs podman, taking 5-10 minutes.
pub async fn cmd_setup(args: SetupArgs) -> Result<()> {
    // Record --config before anything reads config. The --install-host-kernel
    // branch below returns early, so a later placement leaves that path reading
    // the discovered config instead of the requested one.
    if let Some(path) = args.config.as_deref() {
        crate::setup::rootfs::set_config_path(path);
    }

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
            crate::setup::ensure_kernel(profile_name, true, args.build_kernels)
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

    // Build the optional Cloud Hypervisor backend binary (#632) and exit. CH is an
    // optional VMM backend, so it is built on demand via `--cloud-hypervisor` rather
    // than on every setup (a CH build is as slow as a firecracker build).
    if args.cloud_hypervisor {
        let ch = config.cloud_hypervisor.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "--cloud-hypervisor requires a [cloud_hypervisor] section (repo + branch) in the config"
            )
        })?;
        println!("Building Cloud Hypervisor backend...");
        let path = crate::setup::ensure_cloud_hypervisor(&ch.repo, &ch.branch)
            .await
            .context("setting up cloud-hypervisor")?;
        println!("  ✓ Cloud Hypervisor ready: {}", path.display());
        return Ok(());
    }

    println!("Setting up fcvm (this may take 5-10 minutes on first run)...");

    // Ordinary setup downloads the released default artifact. Any source-kernel
    // release job can bootstrap it locally with --build-kernels when the release
    // is not published yet; the per-artifact flock serializes concurrent setup.
    let kernel_path = crate::setup::ensure_kernel("default", true, args.build_kernels)
        .await
        .context("setting up kernel")?;
    println!("  ✓ Kernel ready: {}", kernel_path.display());

    // Build the pinned pasta if configured in [pasta] section (rootless
    // networking requires it when configured — see src/setup/pasta.rs)
    if let Some(path) = crate::setup::ensure_pasta(config.pasta.as_ref())
        .await
        .context("setting up pasta")?
    {
        println!("  ✓ Pasta ready: {}", path.display());
    }

    // Build default Firecracker if configured in [firecracker] section
    // The [firecracker] config is applied to explicit default profiles at load time.
    if config.firecracker.is_some() {
        let default_profile = crate::setup::rootfs::get_kernel_profile("default")?
            .ok_or_else(|| anyhow::anyhow!("default kernel profile not found in config"))?;
        crate::setup::ensure_profile_firecracker(&default_profile, "default")
            .await
            .context("setting up default firecracker")?;
    }

    // Setup kernel profile if requested — must happen BEFORE rootfs creation
    // because btrfs rootfs needs the profile kernel to boot the setup VM
    if let Some(profile_name) = &args.kernel_profile {
        let profile = get_kernel_profile(profile_name)?.ok_or_else(|| {
            anyhow::anyhow!("kernel profile '{}' not found in config", profile_name)
        })?;

        println!(
            "\nSetting up kernel profile '{}': {}",
            profile_name, profile.description
        );

        // Download or build the profile kernel. --force-build-kernels bypasses
        // the download so a release refresh cannot republish the artifact it
        // was asked to replace.
        let profile_kernel_path = if args.force_build_kernels {
            crate::setup::rebuild_kernel_from_source(profile_name)
                .await
                .context("rebuilding profile kernel from source")?
        } else {
            crate::setup::ensure_kernel(profile_name, true, args.build_kernels)
                .await
                .context("setting up profile kernel")?
        };
        println!(
            "  ✓ Profile kernel ready: {}",
            profile_kernel_path.display()
        );

        // Build profile firecracker if needed
        crate::setup::ensure_profile_firecracker(&profile, profile_name)
            .await
            .context("setting up profile firecracker")?;
    }

    // Resolve rootfs type: CLI override > kernel profile config > default (ext4)
    let rootfs_type = crate::setup::resolve_rootfs_type(
        args.rootfs_type.as_ref(),
        args.kernel_profile.as_deref().unwrap_or("default"),
    );

    // Ensure rootfs exists (creates Layer 2 if missing)
    let rootfs_path = crate::setup::ensure_rootfs(true, rootfs_type.as_deref())
        .await
        .context("setting up rootfs")?;
    println!("  ✓ Rootfs ready: {}", rootfs_path.display());

    // Ensure fc-agent initrd exists
    let initrd_path = crate::setup::ensure_fc_agent_initrd(true)
        .await
        .context("setting up fc-agent initrd")?;
    println!("  ✓ Initrd ready: {}", initrd_path.display());

    if let Some(profile_name) = &args.kernel_profile {
        println!("\nFor '{}' profile, use:", profile_name);
        println!(
            "  fcvm podman run --kernel-profile {} --privileged ...",
            profile_name
        );
    }

    println!("\nSetup complete! You can now run VMs with: fcvm podman run ...");

    Ok(())
}
