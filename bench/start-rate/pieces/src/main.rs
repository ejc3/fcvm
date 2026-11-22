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
}

fn main() -> anyhow::Result<()> {
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
    }
}
