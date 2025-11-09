use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::process::Command;

pub async fn setup_rootfs(output: &str, suite: &str, size_mb: u32) -> Result<()> {
    let output_path = expand_path(output)?;

    println!("📦 Creating rootfs image...");
    println!("  Output: {}", output_path.display());
    println!("  Suite: {}", suite);
    println!("  Size: {} MB", size_mb);

    // Create output directory
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent).await
            .context("creating output directory")?;
    }

    // Create temporary directory for building
    let temp_dir = tempfile::tempdir()
        .context("creating temp directory")?;
    let rootfs_dir = temp_dir.path().join("rootfs");

    println!("\n🏗️  Step 1: Building base system with debootstrap...");
    build_base_system(&rootfs_dir, suite).await?;

    println!("\n📦 Step 2: Installing Podman and dependencies...");
    install_podman(&rootfs_dir).await?;

    println!("\n🤖 Step 3: Installing fc-agent...");
    install_fc_agent(&rootfs_dir).await?;

    println!("\n💾 Step 4: Creating ext4 image...");
    create_image(&rootfs_dir, &output_path, size_mb).await?;

    println!("\n✓ Rootfs image ready at: {}", output_path.display());
    Ok(())
}

async fn build_base_system(rootfs_dir: &Path, suite: &str) -> Result<()> {
    // Check if running as root
    let uid_output = Command::new("id")
        .arg("-u")
        .output()
        .await?;
    let uid = String::from_utf8_lossy(&uid_output.stdout).trim().to_string();

    if uid != "0" {
        println!("⚠️  Note: debootstrap requires root. You may be prompted for sudo password.");
    }

    let status = Command::new("sudo")
        .args(&[
            "debootstrap",
            "--variant=minbase",
            suite,
            rootfs_dir.to_str().unwrap(),
            "http://deb.debian.org/debian",
        ])
        .status()
        .await
        .context("running debootstrap")?;

    if !status.success() {
        anyhow::bail!("debootstrap failed");
    }

    Ok(())
}

async fn install_podman(rootfs_dir: &Path) -> Result<()> {
    let packages = vec![
        "systemd",
        "podman",
        "conmon",
        "crun",
        "fuse-overlayfs",
        "uidmap",
        "slirp4netns",
        "iproute2",
        "curl",
        "jq",
        "ca-certificates",
    ];

    let install_script = format!(
        r#"
        apt-get update
        DEBIAN_FRONTEND=noninteractive apt-get install -y {}
        "#,
        packages.join(" ")
    );

    let status = Command::new("sudo")
        .args(&["chroot", rootfs_dir.to_str().unwrap(), "/bin/bash", "-c", &install_script])
        .status()
        .await
        .context("installing podman")?;

    if !status.success() {
        anyhow::bail!("package installation failed");
    }

    // Configure Podman for rootless
    let config_script = r#"
        useradd -m -s /bin/bash podman || true
        echo "podman:100000:65536" >> /etc/subuid
        echo "podman:100000:65536" >> /etc/subgid
    "#;

    Command::new("sudo")
        .args(&["chroot", rootfs_dir.to_str().unwrap(), "/bin/bash", "-c", config_script])
        .status()
        .await
        .context("configuring podman user")?;

    Ok(())
}

async fn install_fc_agent(rootfs_dir: &Path) -> Result<()> {
    // Find fc-agent binary
    let agent_path = find_fc_agent().await?;

    println!("  Found fc-agent at: {}", agent_path.display());

    // Copy fc-agent into rootfs
    let dest = rootfs_dir.join("usr/local/bin/fc-agent");
    if let Some(parent) = dest.parent() {
        Command::new("sudo")
            .args(&["mkdir", "-p", parent.to_str().unwrap()])
            .status()
            .await?;
    }

    Command::new("sudo")
        .args(&["cp", agent_path.to_str().unwrap(), dest.to_str().unwrap()])
        .status()
        .await
        .context("copying fc-agent")?;

    Command::new("sudo")
        .args(&["chmod", "+x", dest.to_str().unwrap()])
        .status()
        .await?;

    // Create systemd service
    let service_content = r#"[Unit]
Description=Firecracker Guest Agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/fc-agent
Restart=on-failure
RestartSec=10s

[Install]
WantedBy=multi-user.target
"#;

    let service_path = rootfs_dir.join("etc/systemd/system/fc-agent.service");
    if let Some(parent) = service_path.parent() {
        Command::new("sudo")
            .args(&["mkdir", "-p", parent.to_str().unwrap()])
            .status()
            .await?;
    }

    fs::write("/tmp/fc-agent.service", service_content).await?;
    Command::new("sudo")
        .args(&["cp", "/tmp/fc-agent.service", service_path.to_str().unwrap()])
        .status()
        .await?;

    // Enable service
    Command::new("sudo")
        .args(&["chroot", rootfs_dir.to_str().unwrap(), "systemctl", "enable", "fc-agent.service"])
        .status()
        .await
        .context("enabling fc-agent service")?;

    Ok(())
}

async fn find_fc_agent() -> Result<PathBuf> {
    // Try common locations
    let candidates = vec![
        PathBuf::from("./fc-agent/target/release/fc-agent"),
        PathBuf::from("./target/release/fc-agent"),
        PathBuf::from("../fc-agent/target/release/fc-agent"),
        PathBuf::from("/usr/local/bin/fc-agent"),
    ];

    for path in candidates {
        if fs::metadata(&path).await.is_ok() {
            return Ok(path);
        }
    }

    anyhow::bail!("fc-agent binary not found. Please build it first: cd fc-agent && cargo build --release");
}

async fn create_image(rootfs_dir: &Path, output: &Path, size_mb: u32) -> Result<()> {
    let image_path = output.with_extension("ext4");

    // Create empty file
    Command::new("dd")
        .args(&[
            "if=/dev/zero",
            &format!("of={}", image_path.display()),
            "bs=1M",
            &format!("count={}", size_mb),
        ])
        .status()
        .await
        .context("creating image file")?;

    // Format as ext4
    Command::new("sudo")
        .args(&["mkfs.ext4", "-F", image_path.to_str().unwrap()])
        .status()
        .await
        .context("formatting image")?;

    // Mount and copy files
    let mount_point = tempfile::tempdir()
        .context("creating mount point")?;

    Command::new("sudo")
        .args(&["mount", image_path.to_str().unwrap(), mount_point.path().to_str().unwrap()])
        .status()
        .await
        .context("mounting image")?;

    // Copy rootfs contents
    Command::new("sudo")
        .args(&["cp", "-a", &format!("{}/*", rootfs_dir.display()), &format!("{}/", mount_point.path().display())])
        .status()
        .await
        .context("copying rootfs contents")?;

    // Unmount
    Command::new("sudo")
        .args(&["umount", mount_point.path().to_str().unwrap()])
        .status()
        .await
        .context("unmounting image")?;

    // Move to final location
    if image_path != *output {
        fs::rename(&image_path, output).await
            .context("moving image to final location")?;
    }

    Ok(())
}

fn expand_path(path: &str) -> Result<PathBuf> {
    let expanded = if path.starts_with("~/") {
        let home = std::env::var("HOME")
            .context("HOME not set")?;
        PathBuf::from(home).join(&path[2..])
    } else {
        PathBuf::from(path)
    };
    Ok(expanded)
}
