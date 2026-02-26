mod agent;
mod container;
mod exec;
mod fuse;
mod lock_test;
mod mmds;
mod mounts;
mod network;
mod output;
mod proxy;
mod restore;
mod system;
mod tty;
mod types;
mod vsock;

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    // Use a non-blocking writer for stderr so that log writes never block the
    // tokio runtime. Without this, heavy FUSE traffic generates thousands of
    // INFO messages/sec which synchronously write to the serial console (virtio),
    // starving the async output handler that drains the container's stdout pipe.
    // The container's entrypoint then deadlocks on pipe write.
    let (non_blocking, _guard) = tracing_appender::non_blocking(std::io::stderr());

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("fc_agent=info,warn")),
        )
        .with_target(true)
        .with_ansi(false)
        .with_writer(non_blocking)
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
