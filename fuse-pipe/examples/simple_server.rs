//! Simple passthrough server example.
//!
//! This example demonstrates how to create a FUSE passthrough server
//! that serves a local directory over a Unix socket.
//!
//! # Usage
//!
//! ```bash
//! # Start the server (serves /tmp as the filesystem)
//! cargo run --example simple_server -- /tmp /tmp/fuse.sock
//!
//! # In another terminal, mount with the client
//! cargo run --example simple_client -- /tmp/fuse.sock /mnt/fuse
//! ```

use fuse_pipe::{AsyncServer, PassthroughFs, ServerConfig};
use std::env;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();

    if args.len() != 3 {
        eprintln!("Usage: {} <directory> <socket_path>", args[0]);
        eprintln!();
        eprintln!("Example:");
        eprintln!("  {} /tmp /tmp/fuse.sock", args[0]);
        std::process::exit(1);
    }

    let directory = &args[1];
    let socket_path = &args[2];

    eprintln!("Creating passthrough filesystem for: {}", directory);
    let fs = PassthroughFs::new(directory);

    eprintln!("Starting server on: {}", socket_path);
    eprintln!("Press Ctrl+C to stop");

    // Use high-throughput config for better performance
    let config = ServerConfig::high_throughput();
    let server = AsyncServer::with_config(fs, config);

    server.serve_unix(socket_path).await
}
