//! Simple FUSE client example.
//!
//! This example demonstrates how to mount a FUSE filesystem that
//! connects to a fuse-pipe server.
//!
//! # Usage
//!
//! ```bash
//! # First, start the server in another terminal:
//! cargo run --example simple_server -- /tmp /tmp/fuse.sock
//!
//! # Create mount point and run client (requires root for FUSE mount):
//! sudo mkdir -p /mnt/fuse
//! sudo cargo run --example simple_client -- /tmp/fuse.sock /mnt/fuse
//!
//! # Now you can access files:
//! ls /mnt/fuse
//! cat /mnt/fuse/some_file
//!
//! # Unmount when done:
//! sudo umount /mnt/fuse
//! ```
//!
//! # Requirements
//!
//! - FUSE kernel module must be loaded
//! - Root privileges required for mounting
//! - `allow_other` option requires `/etc/fuse.conf` with `user_allow_other`

use fuse_pipe::mount;
use std::env;
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();

    if args.len() != 3 {
        eprintln!("Usage: {} <socket_path> <mount_point>", args[0]);
        eprintln!();
        eprintln!("Example:");
        eprintln!("  sudo {} /tmp/fuse.sock /mnt/fuse", args[0]);
        std::process::exit(1);
    }

    let socket_path = &args[1];
    let mount_point = PathBuf::from(&args[2]);

    eprintln!("Connecting to server at: {}", socket_path);
    eprintln!("Mounting at: {:?}", mount_point);
    eprintln!("Press Ctrl+C to unmount");

    // Mount the FUSE filesystem
    mount(socket_path, &mount_point)
}
