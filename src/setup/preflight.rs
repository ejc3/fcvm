use anyhow::Result;
use tokio::process::Command;

pub async fn check_preflight() -> Result<()> {
    println!("🔍 Checking fcvm requirements...\n");

    let mut all_ok = true;

    // Check for Firecracker
    all_ok &= check_command("firecracker", "--version", "Firecracker").await;

    // Check for Podman
    all_ok &= check_command("podman", "--version", "Podman").await;

    // Check for slirp4netns (rootless networking)
    all_ok &= check_command("slirp4netns", "--version", "slirp4netns").await;

    // Check for nftables (privileged networking)
    all_ok &= check_command("nft", "--version", "nftables").await;

    // Check for debootstrap (rootfs creation)
    all_ok &= check_command("debootstrap", "--version", "debootstrap").await;

    // Check KVM support
    all_ok &= check_kvm().await;

    // Check for kernel
    all_ok &= check_kernel().await;

    // Check for rootfs
    all_ok &= check_rootfs().await;

    println!();
    if all_ok {
        println!("✅ All checks passed! You're ready to use fcvm.");
    } else {
        println!("⚠️  Some checks failed. Run setup commands to fix:");
        println!("  fcvm setup kernel --download    # Download kernel");
        println!("  fcvm setup rootfs                # Create rootfs image");
    }

    Ok(())
}

async fn check_command(cmd: &str, arg: &str, name: &str) -> bool {
    print!("  {} ... ", name);

    match Command::new(cmd).arg(arg).output().await {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout);
            let version_line = version.lines().next().unwrap_or("installed");
            println!("✓ {}", version_line.trim());
            true
        }
        _ => {
            println!("✗ not found");
            false
        }
    }
}

async fn check_kvm() -> bool {
    print!("  KVM support ... ");

    if tokio::fs::metadata("/dev/kvm").await.is_ok() {
        println!("✓ available");
        true
    } else {
        println!("✗ /dev/kvm not found");
        false
    }
}

async fn check_kernel() -> bool {
    print!("  Kernel image ... ");

    let paths = vec![
        std::path::PathBuf::from(shellexpand::tilde("~/.local/share/fcvm/images/vmlinux").as_ref()),
        std::path::PathBuf::from("/usr/share/fcvm/vmlinux"),
    ];

    for path in paths {
        if tokio::fs::metadata(&path).await.is_ok() {
            println!("✓ {}", path.display());
            return true;
        }
    }

    println!("✗ not found (run: fcvm setup kernel --download)");
    false
}

async fn check_rootfs() -> bool {
    print!("  Rootfs image ... ");

    let paths = vec![
        std::path::PathBuf::from(shellexpand::tilde("~/.local/share/fcvm/images/rootfs.ext4").as_ref()),
        std::path::PathBuf::from("/usr/share/fcvm/rootfs.ext4"),
    ];

    for path in paths {
        if tokio::fs::metadata(&path).await.is_ok() {
            println!("✓ {}", path.display());
            return true;
        }
    }

    println!("✗ not found (run: fcvm setup rootfs)");
    false
}
