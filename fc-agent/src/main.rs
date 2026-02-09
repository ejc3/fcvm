mod agent;
mod container;
mod exec;
mod fuse;
mod lock_test;
mod mmds;
mod mounts;
mod network;
mod output;
mod restore;
mod system;
mod tty;
mod types;
mod vsock;

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,fuse_pipe=debug")),
        )
        .with_target(true)
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .init();

    eprintln!("[fc-agent] starting");

    if let Err(e) = agent::run().await {
        eprintln!("[fc-agent] ==========================================");
        eprintln!("[fc-agent] FATAL ERROR: Container failed to start");
        eprintln!("[fc-agent] Error: {:?}", e);
        eprintln!("[fc-agent] ==========================================");
        vsock::notify_container_exit(1);
        system::shutdown_vm(1).await;
    }
}
