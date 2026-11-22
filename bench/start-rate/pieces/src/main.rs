//! Runs one piece of what `fcvm snapshot run` does to start a clone, N times at concurrency C,
//! through fcvm's own library code, and reports ops/s, latency and CPU per op
//! (bench/start-rate/pieces/README.md).
//!
//! `drive` is the parent. Each op is one `instance` child process, as each clone is one fcvm
//! process: the child prepares what the piece needs (untimed), reports `ready`, runs the piece
//! when it reads `go`, reports `done`, then holds what it made until it reads `exit` (or its
//! stdin closes), tears it down, reports `torn` and exits. Held instances are the "alive" count.

mod drive;
mod kvm;
mod pieces;
mod tap;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "start-pieces")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a piece COUNT times at CONCURRENCY with ALIVE instances already held.
    Drive(drive::DriveArgs),
    /// One instance of a piece (spawned by `drive`).
    Instance(pieces::PieceArgs),
    /// A minimal HTTP/1.0 responder, run inside a namespace for the health piece.
    HttpServe {
        /// Address to listen on.
        addr: String,
    },
    /// Attach and detach a TAP on request, inside a holder's namespaces, for the tap-attach
    /// piece (spawned by its instance).
    TapHelper {
        /// The instance that spawned this helper.
        parent: u32,
        /// The namespace holder's PID.
        holder_pid: u32,
        /// The TAP device to attach to.
        tap: String,
    },
    /// What a tap-attach cell's `tap.bt` dumps say one attach and detach cost.
    TapSummary {
        /// The cell's results directory.
        cell: std::path::PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    // The namespace holder fcvm's library starts is this binary run as its agent.
    if let Some(result) = fcvm::network::netns_agent::main_if_agent() {
        return result;
    }
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Drive(args) => {
            // Eight workers multiplex thousands of children's pipes; more threads would only
            // make the driver's own spawns dearer.
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(8)
                .enable_all()
                .build()?
                .block_on(drive::run(args))
        }
        Cmd::Instance(args) => {
            // The same runtime shape `fcvm snapshot run` uses (two workers, #1995).
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?
                .block_on(pieces::instance(args))
        }
        Cmd::HttpServe { addr } => pieces::http_serve(&addr),
        Cmd::TapHelper {
            parent,
            holder_pid,
            tap,
        } => tap::helper(parent, holder_pid, &tap),
        Cmd::TapSummary { cell } => tap::summary(&cell),
    }
}

#[cfg(test)]
mod tests {
    /// fcvm's default features are its integration-test tiers. This crate is a default
    /// workspace member, so depending on fcvm with them switched those tests on in
    /// `make test-unit`, which runs `--no-default-features` on a runner with no VM setup:
    /// 207 of them failed on the first CI run of #2006.
    #[test]
    fn fcvm_is_used_without_its_default_features() {
        let manifest = include_str!("../Cargo.toml");
        let line = manifest
            .lines()
            .find(|l| l.starts_with("fcvm = "))
            .expect("an fcvm dependency");
        assert!(line.contains("default-features = false"), "{line}");
    }
}
