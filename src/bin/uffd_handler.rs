// UFFD Handler - Serves memory pages on demand for VM clones
// This enables true page-level copy-on-write memory sharing across VMs
//
// Usage: uffd_handler <socket_path> <mem_file_path>
//
// Based on Firecracker's examples/uffd/on_demand_handler.rs

use std::fs::File;
use std::io;
use std::os::unix::io::{FromRawFd, IntoRawFd};
use std::os::unix::net::UnixListener;

use memmap2::MmapOptions;
use userfaultfd::{Event, Uffd};
use vmm_sys_util::sock_ctrl_msg::ScmSocket;

const PAGE_SIZE: usize = 4096;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() != 3 {
        eprintln!("Usage: {} <socket_path> <mem_file_path>", args[0]);
        eprintln!("  Serves memory pages on-demand via userfaultfd for VM cloning");
        std::process::exit(1);
    }

    let socket_path = &args[1];
    let mem_file_path = &args[2];

    eprintln!("UFFD Handler starting");
    eprintln!("  Socket: {}", socket_path);
    eprintln!("  Memory file: {}", mem_file_path);

    // Open and mmap the memory snapshot file
    let mem_file = File::open(mem_file_path)?;
    let mem_size = mem_file.metadata()?.len() as usize;
    eprintln!("  Memory size: {} MB", mem_size / (1024 * 1024));

    // Safety: We're mapping a read-only file
    let mmap = unsafe {
        MmapOptions::new()
            .len(mem_size)
            .map(&mem_file)?
    };

    eprintln!("  Memory mapped successfully");

    // Bind Unix socket and wait for Firecracker to connect
    let listener = UnixListener::bind(socket_path)?;
    eprintln!("  Listening on socket: {}", socket_path);

    let (mut stream, _) = listener.accept()?;
    eprintln!("  Firecracker connected!");

    // Receive UFFD file descriptor and memory mappings from Firecracker
    let (uffd, mappings) = receive_uffd_and_mappings(&mut stream)?;

    eprintln!("  Received UFFD and {} memory region(s)", mappings.len());
    for mapping in &mappings {
        eprintln!("    Region: base=0x{:x}, size={} KB, offset={}",
            mapping.base_host_virt_addr,
            mapping.size / 1024,
            mapping.offset);
    }

    eprintln!("  Starting page fault handler loop...");

    // Main loop: serve page faults
    let mut fault_count = 0u64;
    loop {
        // Read UFFD event
        match uffd.read_event() {
            Ok(Some(Event::Pagefault { addr, .. })) => {
                fault_count += 1;
                if fault_count % 1000 == 0 {
                    eprintln!("  Served {} page faults", fault_count);
                }

                // Find which memory region this address belongs to
                let fault_page = (addr as usize) & !(PAGE_SIZE - 1);

                let mapping = mappings.iter()
                    .find(|m| m.contains(fault_page as u64))
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("Page fault at unmapped address: 0x{:x}", fault_page)
                        )
                    })?;

                // Calculate offset in the snapshot file
                let offset_in_region = fault_page - (mapping.base_host_virt_addr as usize);
                let offset_in_file = mapping.offset as usize + offset_in_region;

                // Get page data from mmap
                let page_data = &mmap[offset_in_file..offset_in_file + PAGE_SIZE];

                // Copy page to guest memory via UFFD
                unsafe {
                    uffd.copy(
                        page_data.as_ptr() as *const std::ffi::c_void,
                        fault_page as *mut std::ffi::c_void,
                        PAGE_SIZE,
                        true
                    )?;
                }
            }
            Ok(Some(Event::Remove { start, end })) => {
                // Balloon device removed pages - acknowledge removal
                let len = (end as usize) - (start as usize);
                unsafe {
                    uffd.zeropage(start, len, true)?;
                }
            }
            Ok(None) => {
                // No more events (shouldn't happen in blocking mode)
                break;
            }
            Err(e) => {
                eprintln!("Error reading UFFD event: {}", e);
                return Err(e.into());
            }
        }
    }

    eprintln!("UFFD handler shutting down. Served {} page faults total.", fault_count);
    Ok(())
}

#[derive(Debug, serde::Deserialize)]
struct GuestRegionUffdMapping {
    base_host_virt_addr: u64,
    size: usize,
    offset: u64,
}

impl GuestRegionUffdMapping {
    fn contains(&self, addr: u64) -> bool {
        addr >= self.base_host_virt_addr
            && addr < self.base_host_virt_addr + self.size as u64
    }
}

fn receive_uffd_and_mappings(
    stream: &mut std::os::unix::net::UnixStream,
) -> Result<(Uffd, Vec<GuestRegionUffdMapping>), Box<dyn std::error::Error>> {
    // Receive message with UFFD file descriptor from Firecracker
    let mut message_buf = vec![0u8; 4096];
    let (bytes_read, uffd_fd_opt) = stream.recv_with_fd(&mut message_buf)?;

    let uffd_file = uffd_fd_opt
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "No UFFD received"))?;

    message_buf.resize(bytes_read, 0);

    // Parse JSON message containing memory region mappings
    let message = String::from_utf8(message_buf)?;
    let mappings: Vec<GuestRegionUffdMapping> = serde_json::from_str(&message)?;

    // Convert File to Uffd
    let uffd = unsafe { Uffd::from_raw_fd(uffd_file.into_raw_fd()) };

    Ok((uffd, mappings))
}
