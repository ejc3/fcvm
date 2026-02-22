//! fc-mock: Drop-in Firecracker replacement using containers.
//!
//! Speaks the Firecracker REST API on a Unix socket but launches podman
//! containers instead of microVMs. Emulates vsock via Unix sockets.

mod api_server;
mod container;
mod vsock_exec;

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Notify;

fn main() -> Result<()> {
    // Handle --version before anything else (no args parsing needed)
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version") {
        println!("Firecracker v1.14.0-mock");
        return Ok(());
    }

    // Parse --api-sock (required)
    let api_sock = parse_arg(&args, "--api-sock").context("--api-sock is required")?;

    // Parse optional args (all ignored but accepted for compatibility)
    let _log_path = parse_arg(&args, "--log-path");
    let _level = parse_arg(&args, "--level");
    // Flags: --show-level, --show-log-origin, --no-seccomp (just accepted, ignored)

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    tracing::info!("fc-mock starting (api-sock: {})", api_sock);

    // In user namespaces, create a private mount namespace so we can bind-mount
    // a custom resolv.conf later. This must happen while single-threaded (before
    // tokio spawns worker threads). Without this, podman can't resolve DNS for
    // image pulls because /etc/resolv.conf points to 127.0.0.53 (systemd-resolved)
    // which is unreachable in the new network namespace.
    if container::in_user_namespace() {
        setup_private_mount_namespace();
    }

    // Run the async runtime
    let rt = tokio::runtime::Runtime::new().context("creating tokio runtime")?;
    rt.block_on(run(PathBuf::from(api_sock)))
}

async fn run(api_sock: PathBuf) -> Result<()> {
    // Shared shutdown signal
    let shutdown = Arc::new(Notify::new());
    let shutdown_clone = shutdown.clone();

    // Setup signal handlers
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    // Start API server
    let state = api_server::SharedState::new();
    let server_handle =
        api_server::start_server(api_sock.clone(), state.clone(), shutdown.clone()).await?;

    tracing::info!("API server ready on {}", api_sock.display());

    // Wait for shutdown signal or server completion
    tokio::select! {
        _ = sigterm.recv() => {
            tracing::info!("received SIGTERM, shutting down");
        }
        _ = sigint.recv() => {
            tracing::info!("received SIGINT, shutting down");
        }
        result = server_handle => {
            match result {
                Ok(Ok(())) => tracing::info!("server exited normally"),
                Ok(Err(e)) => tracing::error!("server error: {}", e),
                Err(e) => tracing::error!("server task panicked: {}", e),
            }
        }
        _ = shutdown_clone.notified() => {
            tracing::info!("shutdown requested by container exit");
        }
    }

    // Cleanup container
    container::cleanup_container().await;

    tracing::info!("fc-mock exiting");
    Ok(())
}

/// Parse a named argument from argv (e.g., --api-sock /path)
fn parse_arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// Create a private mount namespace so we can later bind-mount a custom
/// resolv.conf without affecting the host.
fn setup_private_mount_namespace() {
    // unshare(CLONE_NEWNS) must be called while single-threaded
    let ret = unsafe { libc::unshare(libc::CLONE_NEWNS) };
    if ret != 0 {
        tracing::warn!(
            "failed to create mount namespace: {} (DNS may not work for image pulls)",
            std::io::Error::last_os_error()
        );
        return;
    }

    // Make all mounts private so bind mounts don't propagate to host
    let root = std::ffi::CString::new("/").unwrap();
    let ret = unsafe {
        libc::mount(
            std::ptr::null(),
            root.as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        )
    };
    if ret != 0 {
        tracing::warn!(
            "failed to set mount propagation: {}",
            std::io::Error::last_os_error()
        );
        return;
    }

    tracing::info!("created private mount namespace for DNS fix");
}
