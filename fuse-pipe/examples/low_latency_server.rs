//! Low-latency server for testing single-threaded performance
//!
//! Run with: cargo run --release --example low_latency_server -- /tmp

use fuse_pipe::server::{AsyncServer, PassthroughFs, ServerConfig};
use std::env;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    let root = args.get(1).map(|s| s.as_str()).unwrap_or("/tmp");
    let socket = "/tmp/low-latency.sock";

    println!("Low-latency server:");
    println!("  Root: {}", root);
    println!("  Socket: {}", socket);

    let fs = PassthroughFs::new(root);

    // Key difference: write_batch_size=1 means flush immediately!
    let config = ServerConfig::low_latency();
    println!("  Config: batch_size={}, batch_timeout={:?}",
        config.write_batch_size, config.write_batch_timeout);

    let server = AsyncServer::with_config(fs, config);
    server.run_blocking(socket)
}
