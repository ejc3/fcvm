use anyhow::{anyhow, Context, Result};
use std::fs::File;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::unix::AsyncFd;
use tokio::net::UnixListener;
use tokio::task::JoinSet;
use tracing::{error, info, warn};

use memmap2::MmapOptions;
use userfaultfd::{Event, Uffd};
use vmm_sys_util::sock_ctrl_msg::ScmSocket;

use crate::paths;

const PAGE_SIZE: usize = 4096;

/// Async UFFD server that serves memory pages for multiple VMs from a single snapshot
pub struct UffdServer {
    snapshot_id: String,
    socket_path: PathBuf,
    mmap: Arc<memmap2::Mmap>,
}

impl UffdServer {
    /// Create a new UFFD server for a snapshot
    pub async fn new(snapshot_id: String, mem_file_path: &Path) -> Result<Self> {
        let socket_path = paths::base_dir().join(format!("uffd-{}.sock", snapshot_id));
        Self::new_with_path(snapshot_id, mem_file_path, &socket_path).await
    }

    /// Create a new UFFD server with custom socket path
    pub async fn new_with_path(
        snapshot_id: String,
        mem_file_path: &Path,
        socket_path: &Path,
    ) -> Result<Self> {
        info!(
            target: "uffd",
            snapshot = %snapshot_id,
            mem_file = %mem_file_path.display(),
            socket = %socket_path.display(),
            "creating UFFD server"
        );

        // Open and mmap the memory snapshot file (shared across all VMs)
        let mem_file = File::open(mem_file_path).context("opening memory file")?;
        let mem_size = mem_file.metadata()?.len() as usize;

        info!(
            target: "uffd",
            mem_size_mb = mem_size / (1024 * 1024),
            "mapping memory file"
        );

        // Safety: We're mapping a read-only file for serving pages
        let mmap = Arc::new(unsafe {
            MmapOptions::new()
                .len(mem_size)
                .map(&mem_file)
                .context("mmapping memory file")?
        });

        // Ensure parent directory exists
        if let Some(parent) = socket_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .context("creating socket directory")?;
        }

        // Remove stale socket (ignore errors if not exists - avoids TOCTOU race)
        let _ = tokio::fs::remove_file(&socket_path).await;

        Ok(Self {
            snapshot_id,
            socket_path: socket_path.to_path_buf(),
            mmap,
        })
    }

    /// Get the socket path for this server
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Run the UFFD server (blocks until all VMs disconnect)
    pub async fn run(&self) -> Result<()> {
        info!(
            target: "uffd",
            snapshot = %self.snapshot_id,
            socket = %self.socket_path.display(),
            "starting UFFD server"
        );

        // Bind Unix socket
        let listener = UnixListener::bind(&self.socket_path).context("binding Unix socket")?;

        info!(target: "uffd", "UFFD server listening, waiting for VM connections...");

        let mut vm_tasks: JoinSet<String> = JoinSet::new();
        let mut next_vm_id = 0u64;

        loop {
            tokio::select! {
                // Accept new VM connections
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, _)) => {
                            let vm_id = format!("vm-{}", next_vm_id);
                            next_vm_id += 1;

                            info!(target: "uffd", vm_id = %vm_id, "new VM connection");

                            // Convert tokio UnixStream to std UnixStream for SCM_RIGHTS
                            // IMPORTANT: tokio sockets are non-blocking, but recv_with_fd needs
                            // blocking mode to wait for Firecracker to send the UFFD fd.
                            // Without this, recvmsg returns EAGAIN immediately if data isn't ready.
                            let mut std_stream = stream.into_std()
                                .context("converting to std stream")?;
                            std_stream.set_nonblocking(false)
                                .context("setting socket to blocking mode")?;

                            // Receive UFFD and mappings for this VM
                            match receive_uffd_and_mappings(&mut std_stream) {
                                Ok((uffd, mappings)) => {
                                    info!(
                                        target: "uffd",
                                        vm_id = %vm_id,
                                        regions = mappings.len(),
                                        "received UFFD with {} memory regions",
                                        mappings.len()
                                    );

                                    // Spawn task to handle this VM's page faults
                                    let mmap = Arc::clone(&self.mmap);
                                    let vm_id_clone = vm_id.clone();
                                    vm_tasks.spawn(async move {
                                        match handle_vm_page_faults(vm_id_clone.clone(), uffd, mappings, mmap).await {
                                            Ok(()) => info!(target: "uffd", vm_id = %vm_id_clone, "VM handler exited cleanly"),
                                            Err(e) => error!(target: "uffd", vm_id = %vm_id_clone, error = ?e, "VM handler error"),
                                        }
                                        vm_id_clone
                                    });

                                    info!(target: "uffd", active_vms = vm_tasks.len(), "VM connected");
                                }
                                Err(e) => {
                                    // Log full error chain for debugging (includes syscall errors)
                                    error!(target: "uffd", vm_id = %vm_id, error = ?e, "failed to receive UFFD");
                                }
                            }
                        }
                        Err(e) => {
                            error!(target: "uffd", error = %e, "failed to accept connection");
                        }
                    }
                }

                // Handle completed VM tasks
                Some(result) = vm_tasks.join_next() => {
                    match result {
                        Ok(vm_id) => info!(target: "uffd", vm_id = %vm_id, "VM disconnected"),
                        Err(e) => error!(target: "uffd", error = %e, "VM task panicked"),
                    }

                    info!(target: "uffd", active_vms = vm_tasks.len(), "VM exited");

                    // Exit when last VM disconnects
                    if vm_tasks.is_empty() {
                        info!(target: "uffd", "no active VMs remaining, shutting down server");
                        break;
                    }
                }
            }
        }

        info!(target: "uffd", "UFFD server stopped");
        Ok(())
    }
}

impl Drop for UffdServer {
    fn drop(&mut self) {
        // Clean up socket file (ignore errors - avoids TOCTOU race and handles concurrent cleanup)
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Handle page faults for a single VM
async fn handle_vm_page_faults(
    vm_id: String,
    uffd: Uffd,
    mappings: Vec<GuestRegionUffdMapping>,
    mmap: Arc<memmap2::Mmap>,
) -> Result<()> {
    info!(target: "uffd", vm_id = %vm_id, "page fault handler started");

    // Set UFFD to non-blocking mode for async integration
    let uffd_fd = uffd.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(uffd_fd, libc::F_GETFL);
        libc::fcntl(uffd_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    // Wrap UFFD in AsyncFd for tokio integration
    let async_uffd = AsyncFd::new(uffd).context("creating AsyncFd for UFFD")?;

    let mut fault_count = 0u64;
    let start_time = std::time::Instant::now();

    loop {
        // Wait for UFFD to be readable
        let mut guard = match async_uffd.readable().await {
            Ok(guard) => guard,
            Err(e) => {
                error!(target: "uffd", vm_id = %vm_id, error = %e, "error waiting for UFFD readability");
                return Err(e.into());
            }
        };

        // Read all available events (non-blocking)
        loop {
            let event = match guard.get_inner().read_event() {
                Ok(Some(event)) => event,
                Ok(None) => break, // No more events ready
                Err(_) => {
                    // UFFD closed = VM exited
                    let elapsed = start_time.elapsed();
                    let rate = if elapsed.as_secs_f64() > 0.0 {
                        fault_count as f64 / elapsed.as_secs_f64()
                    } else {
                        0.0
                    };
                    info!(
                        target: "uffd",
                        vm_id = %vm_id,
                        fault_count,
                        elapsed_secs = format!("{:.1}", elapsed.as_secs_f64()),
                        pages_per_sec = format!("{:.0}", rate),
                        "VM exited"
                    );
                    return Ok(());
                }
            };

            match event {
                Event::Pagefault { addr, .. } => {
                    fault_count += 1;

                    // Find which memory region this address belongs to
                    let fault_page = (addr as usize) & !(PAGE_SIZE - 1);

                    let mapping = mappings
                        .iter()
                        .find(|m| m.contains(fault_page as u64))
                        .ok_or_else(|| {
                            anyhow::anyhow!("page fault at unmapped address: 0x{:x}", fault_page)
                        })?;

                    let base_host = mapping.base_host_virt_addr as usize;
                    if fault_page < base_host {
                        return Err(anyhow!(
                            "page fault address 0x{:x} precedes mapping base 0x{:x}",
                            fault_page,
                            base_host
                        ));
                    }

                    let offset_in_region = fault_page - base_host;
                    let mapping_offset = usize::try_from(mapping.offset)
                        .map_err(|_| anyhow!("mapping offset exceeds host address space"))?;
                    let offset_in_file = mapping_offset
                        .checked_add(offset_in_region)
                        .ok_or_else(|| anyhow!("mapping offset overflow"))?;
                    let mmap_len = mmap.len();

                    if offset_in_file >= mmap_len {
                        warn!(
                            target: "uffd",
                            vm_id = %vm_id,
                            fault_addr = format!("0x{:x}", fault_page),
                            "page fault past end of snapshot memory, zero-filling page"
                        );
                        let zero_page = [0u8; PAGE_SIZE];
                        let result = unsafe {
                            guard.get_inner().copy(
                                zero_page.as_ptr() as *const std::ffi::c_void,
                                fault_page as *mut std::ffi::c_void,
                                PAGE_SIZE,
                                true,
                            )
                        };
                        if let Err(e) = result {
                            error!(
                                target: "uffd",
                                vm_id = %vm_id,
                                fault_addr = format!("0x{:x}", fault_page),
                                error = ?e,
                                "UFFD zero-page copy failed"
                            );
                            return Err(e.into());
                        }
                        continue;
                    }

                    let bytes_available = mmap_len - offset_in_file;

                    let copy_result = if bytes_available >= PAGE_SIZE {
                        let page_data = &mmap[offset_in_file..offset_in_file + PAGE_SIZE];
                        unsafe {
                            guard.get_inner().copy(
                                page_data.as_ptr() as *const std::ffi::c_void,
                                fault_page as *mut std::ffi::c_void,
                                PAGE_SIZE,
                                true,
                            )
                        }
                    } else {
                        let mut temp = [0u8; PAGE_SIZE];
                        temp[..bytes_available].copy_from_slice(
                            &mmap[offset_in_file..offset_in_file + bytes_available],
                        );
                        unsafe {
                            guard.get_inner().copy(
                                temp.as_ptr() as *const std::ffi::c_void,
                                fault_page as *mut std::ffi::c_void,
                                PAGE_SIZE,
                                true,
                            )
                        }
                    };

                    if let Err(e) = copy_result {
                        // Log detailed error info for debugging (use Debug format to show errno)
                        error!(
                            target: "uffd",
                            vm_id = %vm_id,
                            fault_addr = format!("0x{:x}", fault_page),
                            offset_in_file,
                            error = ?e,
                            "UFFD copy failed"
                        );
                        return Err(e.into());
                    }
                }
                Event::Remove { start, end } => {
                    // Balloon device removed pages - zero them
                    // Validate bounds: end must be >= start and range must be reasonable
                    let start_addr = start as usize;
                    let end_addr = end as usize;
                    if end_addr < start_addr {
                        warn!(
                            target: "uffd",
                            vm_id = %vm_id,
                            start = format!("0x{:x}", start_addr),
                            end = format!("0x{:x}", end_addr),
                            "Remove event with invalid range (end < start), ignoring"
                        );
                        continue;
                    }
                    let len = end_addr.saturating_sub(start_addr);
                    if len == 0 {
                        continue; // Nothing to zero
                    }
                    unsafe {
                        guard.get_inner().zeropage(start, len, true)?;
                    }
                }
                Event::Fork { .. } | Event::Remap { .. } | Event::Unmap { .. } => {
                    // Ignore these events
                }
            }
        }

        // Clear readiness
        guard.clear_ready();
    }
}

/// Memory region mapping from Firecracker
#[derive(Debug, serde::Deserialize)]
struct GuestRegionUffdMapping {
    base_host_virt_addr: u64,
    size: usize,
    offset: u64,
}

impl GuestRegionUffdMapping {
    /// Check if address is within this mapping (overflow-safe)
    fn contains(&self, addr: u64) -> bool {
        if addr < self.base_host_virt_addr {
            return false;
        }
        // Use checked arithmetic to prevent overflow
        match self.base_host_virt_addr.checked_add(self.size as u64) {
            Some(end) => addr < end,
            None => true, // If overflow, assume addr is within (max range)
        }
    }

    /// Validate that this mapping has sensible values
    fn validate(&self) -> Result<()> {
        if self.size == 0 {
            anyhow::bail!(
                "mapping has zero size at base 0x{:x}",
                self.base_host_virt_addr
            );
        }
        // Check for overflow in base + size
        if self
            .base_host_virt_addr
            .checked_add(self.size as u64)
            .is_none()
        {
            anyhow::bail!(
                "mapping range overflow: base 0x{:x}, size {}",
                self.base_host_virt_addr,
                self.size
            );
        }
        Ok(())
    }
}

/// Receive UFFD descriptor and memory mappings from Firecracker over Unix socket
fn receive_uffd_and_mappings(
    stream: &mut std::os::unix::net::UnixStream,
) -> Result<(Uffd, Vec<GuestRegionUffdMapping>)> {
    // Receive message with UFFD file descriptor from Firecracker
    let mut message_buf = vec![0u8; 4096];
    let (bytes_read, uffd_fd_opt) = stream
        .recv_with_fd(&mut message_buf)
        .context("receiving UFFD from Firecracker")?;

    let uffd_file =
        uffd_fd_opt.ok_or_else(|| anyhow::anyhow!("no UFFD file descriptor received"))?;

    message_buf.resize(bytes_read, 0);

    // Parse JSON message containing memory region mappings
    let message = String::from_utf8(message_buf).context("parsing message as UTF-8")?;
    let mappings: Vec<GuestRegionUffdMapping> =
        serde_json::from_str(&message).context("parsing memory mappings JSON")?;

    // Validate all received mappings
    if mappings.is_empty() {
        anyhow::bail!("received empty memory mappings from Firecracker");
    }
    for (i, mapping) in mappings.iter().enumerate() {
        mapping
            .validate()
            .with_context(|| format!("invalid mapping at index {}", i))?;
    }

    // Convert File to Uffd
    let uffd = unsafe { Uffd::from_raw_fd(uffd_file.into_raw_fd()) };

    Ok((uffd, mappings))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mapping_contains_basic() {
        let mapping = GuestRegionUffdMapping {
            base_host_virt_addr: 0x1000,
            size: 0x1000, // 4KB
            offset: 0,
        };

        // Before mapping
        assert!(!mapping.contains(0x0FFF));
        // Start of mapping
        assert!(mapping.contains(0x1000));
        // Middle of mapping
        assert!(mapping.contains(0x1500));
        // Last byte of mapping
        assert!(mapping.contains(0x1FFF));
        // Just after mapping
        assert!(!mapping.contains(0x2000));
        // Way after mapping
        assert!(!mapping.contains(0x3000));
    }

    #[test]
    fn test_mapping_contains_large_address() {
        // Test with addresses near u64::MAX to verify overflow handling
        let mapping = GuestRegionUffdMapping {
            base_host_virt_addr: u64::MAX - 0x1000,
            size: 0x800,
            offset: 0,
        };

        // Should contain addresses within range
        assert!(mapping.contains(u64::MAX - 0x1000));
        assert!(mapping.contains(u64::MAX - 0x900));

        // Should not contain addresses before range
        assert!(!mapping.contains(u64::MAX - 0x1001));
    }

    #[test]
    fn test_mapping_contains_overflow() {
        // Test case where base + size would overflow u64
        let mapping = GuestRegionUffdMapping {
            base_host_virt_addr: u64::MAX - 100,
            size: 200, // This would overflow
            offset: 0,
        };

        // With overflow, contains() returns true for addresses >= base
        assert!(mapping.contains(u64::MAX - 100));
        assert!(mapping.contains(u64::MAX));
        // Still false for addresses before base
        assert!(!mapping.contains(u64::MAX - 101));
    }

    #[test]
    fn test_mapping_validate_success() {
        let mapping = GuestRegionUffdMapping {
            base_host_virt_addr: 0x1000,
            size: 0x1000,
            offset: 0,
        };
        assert!(mapping.validate().is_ok());
    }

    #[test]
    fn test_mapping_validate_zero_size() {
        let mapping = GuestRegionUffdMapping {
            base_host_virt_addr: 0x1000,
            size: 0, // Invalid
            offset: 0,
        };
        let result = mapping.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("zero size"));
    }

    #[test]
    fn test_mapping_validate_overflow() {
        let mapping = GuestRegionUffdMapping {
            base_host_virt_addr: u64::MAX - 100,
            size: 200, // Would overflow
            offset: 0,
        };
        let result = mapping.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("overflow"));
    }

    #[test]
    fn test_mapping_json_parsing() {
        let json = r#"[
            {"base_host_virt_addr": 140000000, "size": 536870912, "offset": 0}
        ]"#;
        let mappings: Vec<GuestRegionUffdMapping> = serde_json::from_str(json).unwrap();
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].base_host_virt_addr, 140000000);
        assert_eq!(mappings[0].size, 536870912); // 512MB
        assert_eq!(mappings[0].offset, 0);
    }

    #[test]
    fn test_mapping_json_multiple_regions() {
        let json = r#"[
            {"base_host_virt_addr": 140000000, "size": 268435456, "offset": 0},
            {"base_host_virt_addr": 408435456, "size": 268435456, "offset": 268435456}
        ]"#;
        let mappings: Vec<GuestRegionUffdMapping> = serde_json::from_str(json).unwrap();
        assert_eq!(mappings.len(), 2);

        // First region
        assert_eq!(mappings[0].size, 268435456); // 256MB
        assert_eq!(mappings[0].offset, 0);

        // Second region
        assert_eq!(mappings[1].offset, 268435456); // Starts after first
    }
}
