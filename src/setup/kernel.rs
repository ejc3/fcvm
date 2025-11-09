use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::process::Command;

pub async fn setup_kernel(output: &str, download: bool) -> Result<()> {
    let output_path = expand_path(output)?;

    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent).await
            .context("creating output directory")?;
    }

    if download {
        download_kernel(&output_path).await?;
    } else {
        extract_kernel(&output_path).await?;
    }

    println!("✓ Kernel ready at: {}", output_path.display());
    Ok(())
}

async fn extract_kernel(output: &Path) -> Result<()> {
    println!("📦 Extracting kernel from host...");

    // Find the latest kernel in /boot
    let ls_output = Command::new("ls")
        .args(&["-t", "/boot/vmlinuz-*"])
        .output()
        .await
        .context("listing kernels")?;

    if !ls_output.status.success() {
        anyhow::bail!("No kernel found in /boot. Try --download instead.");
    }

    let kernel_list = String::from_utf8_lossy(&ls_output.stdout);
    let latest_kernel = kernel_list
        .lines()
        .next()
        .context("no kernel found")?;

    println!("  Found: {}", latest_kernel);
    println!("  Copying to: {}", output.display());

    fs::copy(latest_kernel, output).await
        .context("copying kernel")?;

    Ok(())
}

async fn download_kernel(output: &Path) -> Result<()> {
    println!("📥 Downloading pre-built kernel...");

    // Download Firecracker's hello-vmlinux.bin (minimal kernel for testing)
    let url = "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.10/x86_64/vmlinux-5.10.217";

    println!("  URL: {}", url);
    println!("  Downloading to: {}", output.display());

    let response = reqwest::get(url).await
        .context("downloading kernel")?;

    if !response.status().is_success() {
        anyhow::bail!("Download failed: {}", response.status());
    }

    let bytes = response.bytes().await
        .context("reading response")?;

    fs::write(output, bytes).await
        .context("writing kernel")?;

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
