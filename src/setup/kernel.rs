use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{info, warn};

/// Ensure kernel exists, extracting from host if needed
pub async fn ensure_kernel() -> Result<PathBuf> {
    let kernel_dir = PathBuf::from("/var/lib/fcvm/kernels");
    let kernel_path = kernel_dir.join("vmlinux.bin");
    
    if kernel_path.exists() {
        info!(path = %kernel_path.display(), "kernel already exists");
        return Ok(kernel_path);
    }
    
    println!("⚙️  Setting up kernel (first run)...");
    
    // Create directory
    tokio::fs::create_dir_all(&kernel_dir).await
        .context("creating kernel directory")?;
    
    // Find host kernel
    let host_kernel = find_host_kernel()
        .context("finding host kernel")?;
    
    info!(host_kernel = %host_kernel.display(), "found host kernel");
    println!("  → Extracting from {}...", host_kernel.display());
    
    // Extract kernel
    extract_kernel(&host_kernel, &kernel_path).await
        .context("extracting kernel")?;
    
    println!("  ✓ Kernel ready");
    
    Ok(kernel_path)
}

/// Find host kernel in /boot
fn find_host_kernel() -> Result<PathBuf> {
    // Try current running kernel first
    let uname_output = Command::new("uname")
        .arg("-r")
        .output()
        .context("running uname -r")?;
    
    let kernel_version = String::from_utf8_lossy(&uname_output.stdout)
        .trim()
        .to_string();
    
    let kernel_path = PathBuf::from(format!("/boot/vmlinuz-{}", kernel_version));
    
    if kernel_path.exists() {
        return Ok(kernel_path);
    }
    
    // Fallback: find any vmlinuz in /boot
    let boot_dir = std::fs::read_dir("/boot")
        .context("reading /boot directory")?;
    
    for entry in boot_dir {
        let entry = entry?;
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        
        if name.starts_with("vmlinuz") && !name.contains("rescue") {
            return Ok(entry.path());
        }
    }
    
    bail!("no kernel found in /boot")
}

/// Extract uncompressed kernel from potentially compressed vmlinuz
async fn extract_kernel(src: &Path, dst: &Path) -> Result<()> {
    // Most modern kernels are self-extracting ELF with embedded compressed payload
    // We need the uncompressed ELF
    
    // Try using extract-vmlinux script if available
    if let Ok(output) = Command::new("which")
        .arg("extract-vmlinux")
        .output()
    {
        if output.status.success() {
            info!("using extract-vmlinux script");
            let output = Command::new("extract-vmlinux")
                .arg(src)
                .output()
                .context("running extract-vmlinux")?;
            
            if output.status.success() {
                tokio::fs::write(dst, &output.stdout).await
                    .context("writing extracted kernel")?;
                return Ok(());
            }
        }
    }
    
    warn!("extract-vmlinux not available, trying direct copy");
    
    // Fallback: Just copy the kernel as-is
    // Modern vmlinuz files often work directly with Firecracker
    tokio::fs::copy(src, dst).await
        .context("copying kernel file")?;
    
    Ok(())
}
