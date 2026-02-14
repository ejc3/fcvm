use anyhow::{anyhow, Context, Result};
use std::fs::File;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::unix::AsyncFd;
use tokio::net::UnixListener;
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};

use memmap2::MmapOptions;
use userfaultfd::{Event, Uffd};
use vmm_sys_util::sock_ctrl_msg::ScmSocket;

use crate::paths;

/// Async UFFD server that serves memory pages for multiple VMs from a single snapshot
pub struct UffdServer {
    snapshot_id: String,
    socket_path: PathBuf,
    mmap: Arc<memmap2::Mmap>,
}

impl UffdServer {
    /// Create a new UFFD server for a snapshot
    pub async fn new(snapshot_id: String, mem_file_path: &Path) -> Result<Self> {
        let socket_path = paths::data_dir().join(format!("uffd-{}.sock", snapshot_id));
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

    /// Run the UFFD server (blocks until cancelled via CancellationToken)
    pub async fn run(&self, cancel: tokio_util::sync::CancellationToken) -> Result<()> {
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
                            let mmap = Arc::clone(&self.mmap);

                            info!(target: "uffd", vm_id = %vm_id, "new VM connection");

                            // Spawn per-connection task so the accept loop returns
                            // immediately — no blocking on slow/misbehaving clones.
                            vm_tasks.spawn(async move {
                                match tokio::time::timeout(
                                    Duration::from_secs(30),
                                    receive_uffd_async(stream),
                                ).await {
                                    Ok(Ok((uffd, mappings))) => {
                                        info!(
                                            target: "uffd",
                                            vm_id = %vm_id,
                                            regions = mappings.len(),
                                            "received UFFD with {} memory regions",
                                            mappings.len()
                                        );
                                        match handle_vm_page_faults(vm_id.clone(), uffd, mappings, mmap).await {
                                            Ok(()) => info!(target: "uffd", vm_id = %vm_id, "VM handler exited cleanly"),
                                            Err(e) => error!(target: "uffd", vm_id = %vm_id, error = ?e, "VM handler error"),
                                        }
                                    }
                                    Ok(Err(e)) => {
                                        error!(target: "uffd", vm_id = %vm_id, error = ?e, "handshake failed");
                                    }
                                    Err(_) => {
                                        error!(target: "uffd", vm_id = %vm_id, "handshake timed out after 30s");
                                    }
                                }
                                vm_id
                            });

                            info!(target: "uffd", active_vms = vm_tasks.len(), "VM connection spawned");
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
                }

                // Shut down when cancellation token is triggered (Ctrl-C / SIGTERM)
                _ = cancel.cancelled() => {
                    info!(target: "uffd", "cancellation requested, shutting down server");
                    break;
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
    // Derive page size from mappings (all regions use the same page size)
    let page_size = mappings.first().map(|m| m.page_size).unwrap_or(4096);
    let page_mask = !(page_size - 1);

    info!(
        target: "uffd",
        vm_id = %vm_id,
        page_size,
        "page fault handler started"
    );

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
                    let fault_page = (addr as usize) & page_mask;

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
                        // Heap-allocate zero buffer (2MB on stack would overflow for hugepages)
                        let zero_page: Vec<u8> = vec![0u8; page_size];
                        let result = unsafe {
                            guard.get_inner().copy(
                                zero_page.as_ptr() as *const std::ffi::c_void,
                                fault_page as *mut std::ffi::c_void,
                                page_size,
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

                    let copy_result = if bytes_available >= page_size {
                        let page_data = &mmap[offset_in_file..offset_in_file + page_size];
                        unsafe {
                            guard.get_inner().copy(
                                page_data.as_ptr() as *const std::ffi::c_void,
                                fault_page as *mut std::ffi::c_void,
                                page_size,
                                true,
                            )
                        }
                    } else {
                        // Partial page at end of file: copy available data, zero-fill rest
                        // Heap-allocate (2MB on stack would overflow for hugepages)
                        let mut temp: Vec<u8> = vec![0u8; page_size];
                        temp[..bytes_available].copy_from_slice(
                            &mmap[offset_in_file..offset_in_file + bytes_available],
                        );
                        unsafe {
                            guard.get_inner().copy(
                                temp.as_ptr() as *const std::ffi::c_void,
                                fault_page as *mut std::ffi::c_void,
                                page_size,
                                true,
                            )
                        }
                    };

                    if let Err(e) = copy_result {
                        // EEXIST means page was already filled (race with another fault for same page)
                        // This is normal on older kernels with less aggressive page fault coalescing.
                        // See: https://docs.kernel.org/admin-guide/mm/userfaultfd.html
                        // "the kernel must cope with it returning -EEXIST from ioctl(UFFDIO_COPY) as expected"
                        if let userfaultfd::Error::CopyFailed(errno) = &e {
                            // Compare raw errno value since we may have different nix versions
                            if (*errno as i32) == libc::EEXIST {
                                debug!(
                                    target: "uffd",
                                    vm_id = %vm_id,
                                    fault_addr = format!("0x{:x}", fault_page),
                                    "UFFD copy skipped - page already filled (EEXIST)"
                                );
                                continue;
                            }
                        }

                        // Real error - log with Debug format to show errno
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

/// Memory region mapping from Firecracker.
///
/// Firecracker sends these in the UFFD handshake JSON. The `page_size` field
/// indicates the page granularity for this region:
/// - 4096 (4KB): standard pages
/// - 2097152 (2MB): hugepage-backed memory (`huge_pages: "2M"`)
/// - 16384 (16KB): ARM64 with CONFIG_ARM64_16K_PAGES (future)
///
/// The `page_size` field is required — our Firecracker fork always sends it.
#[derive(Debug, serde::Deserialize)]
struct GuestRegionUffdMapping {
    base_host_virt_addr: u64,
    size: usize,
    offset: u64,
    /// Page size for this region (from Firecracker handshake).
    /// Standard: 4096, hugepages: 2097152.
    page_size: usize,
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
        // Validate page_size is a non-zero power of two
        if self.page_size == 0 || !self.page_size.is_power_of_two() {
            anyhow::bail!(
                "invalid page_size {}: must be a non-zero power of two",
                self.page_size
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

/// Receive UFFD descriptor and memory mappings asynchronously using AsyncFd.
///
/// Uses the same AsyncFd pattern as `handle_vm_page_faults` — waits for readability
/// on the Unix socket, then calls `recv_with_fd` which succeeds immediately since
/// data is ready. This avoids blocking the tokio runtime.
async fn receive_uffd_async(
    stream: tokio::net::UnixStream,
) -> Result<(Uffd, Vec<GuestRegionUffdMapping>)> {
    let std_stream = stream.into_std().context("converting to std stream")?;
    // Keep non-blocking — AsyncFd handles readiness
    let async_stream = AsyncFd::new(std_stream).context("creating AsyncFd for handshake socket")?;
    // 4096 bytes for JSON message buffer (unrelated to page size)
    let mut message_buf = vec![0u8; 4096];

    // Wait for data to arrive, then recv with fd passing
    let (bytes_read, uffd_fd_opt) = loop {
        let mut guard = async_stream.readable().await?;
        match guard.get_inner().recv_with_fd(&mut message_buf) {
            Ok(result) => break result,
            Err(e) if e.errno() == libc::EWOULDBLOCK || e.errno() == libc::EAGAIN => {
                guard.clear_ready();
                continue;
            }
            Err(e) => return Err(e).context("receiving UFFD from Firecracker"),
        }
    };

    let uffd_file = uffd_fd_opt.ok_or_else(|| anyhow!("no UFFD file descriptor received"))?;

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
            page_size: 4096,
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
            page_size: 4096,
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
            page_size: 4096,
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
            page_size: 4096,
        };
        assert!(mapping.validate().is_ok());
    }

    #[test]
    fn test_mapping_validate_zero_size() {
        let mapping = GuestRegionUffdMapping {
            base_host_virt_addr: 0x1000,
            size: 0, // Invalid
            offset: 0,
            page_size: 4096,
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
            page_size: 4096,
        };
        let result = mapping.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("overflow"));
    }

    #[test]
    fn test_mapping_validate_invalid_page_size() {
        let mapping = GuestRegionUffdMapping {
            base_host_virt_addr: 0x1000,
            size: 0x1000,
            offset: 0,
            page_size: 0,
        };
        let result = mapping.validate();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("invalid page_size"));

        let mapping = GuestRegionUffdMapping {
            base_host_virt_addr: 0x1000,
            size: 0x1000,
            offset: 0,
            page_size: 123, // Not a power of two
        };
        let result = mapping.validate();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("invalid page_size"));
    }

    #[test]
    fn test_mapping_json_with_page_size() {
        // Firecracker sends page_size in UFFD handshake
        let json = r#"[
            {"base_host_virt_addr": 140000000, "size": 536870912, "offset": 0, "page_size": 2097152}
        ]"#;
        let mappings: Vec<GuestRegionUffdMapping> = serde_json::from_str(json).unwrap();
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].base_host_virt_addr, 140000000);
        assert_eq!(mappings[0].size, 536870912); // 512MB
        assert_eq!(mappings[0].offset, 0);
        assert_eq!(mappings[0].page_size, 2097152); // 2MB hugepages
    }

    #[test]
    fn test_mapping_json_with_4k_page_size() {
        let json = r#"[
            {"base_host_virt_addr": 140000000, "size": 536870912, "offset": 0, "page_size": 4096}
        ]"#;
        let mappings: Vec<GuestRegionUffdMapping> = serde_json::from_str(json).unwrap();
        assert_eq!(mappings[0].page_size, 4096);
    }

    #[test]
    fn test_mapping_json_with_16k_page_size() {
        // Future-proofing: ARM64 CONFIG_ARM64_16K_PAGES
        let json = r#"[
            {"base_host_virt_addr": 140000000, "size": 536870912, "offset": 0, "page_size": 16384}
        ]"#;
        let mappings: Vec<GuestRegionUffdMapping> = serde_json::from_str(json).unwrap();
        assert_eq!(mappings[0].page_size, 16384);
    }

    #[test]
    fn test_mapping_json_multiple_regions() {
        let json = r#"[
            {"base_host_virt_addr": 140000000, "size": 268435456, "offset": 0, "page_size": 4096},
            {"base_host_virt_addr": 408435456, "size": 268435456, "offset": 268435456, "page_size": 4096}
        ]"#;
        let mappings: Vec<GuestRegionUffdMapping> = serde_json::from_str(json).unwrap();
        assert_eq!(mappings.len(), 2);

        // First region
        assert_eq!(mappings[0].size, 268435456); // 256MB
        assert_eq!(mappings[0].offset, 0);

        // Second region
        assert_eq!(mappings[1].offset, 268435456); // Starts after first
    }

    #[test]
    fn test_mapping_contains_with_hugepage_alignment() {
        // 2MB-aligned mapping
        let mapping = GuestRegionUffdMapping {
            base_host_virt_addr: 0x200000, // 2MB aligned
            size: 0x200000,                // 2MB
            offset: 0,
            page_size: 2097152,
        };
        assert!(mapping.contains(0x200000));
        assert!(mapping.contains(0x300000));
        assert!(!mapping.contains(0x400000));
    }
}
