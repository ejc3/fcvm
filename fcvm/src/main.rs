mod lib;
mod cli;

use anyhow::{Result, bail};
use clap::Parser;
use cli::{Cli, Commands, RunArgs, CloneArgs, NameArgs};
use lib::{Mode};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Commands::Run(args) => cmd_run(args).await,
        Commands::Clone(args) => cmd_clone(args).await,
        Commands::Stop(args) => { println!("(stub) stop {:?}", args.name); Ok(()) },
        Commands::Ls => { println!("(stub) ls"); Ok(()) },
        Commands::Inspect(args) => { println!("(stub) inspect {:?}", args.name); Ok(()) },
        Commands::Logs(args) => { println!("(stub) logs {:?}", args.name); Ok(()) },
        Commands::Top => { println!("(stub) top"); Ok(()) },
    }
}

async fn cmd_run(args: RunArgs) -> Result<()> {
    println!("(stub) fcvm run");
    println!("  image: {}", args.image);
    println!("  name: {:?}", args.name);
    println!("  cpu: {}, mem: {} MiB", args.cpu, args.mem);
    println!("  mode: {:?}", args.mode);
    println!("  map-mode: {:?}", args.map_mode);
    println!("  maps: {:?}", args.map);
    println!("  env: {:?}", args.env);
    println!("  cmd: {:?}", args.cmd);
    println!("  publish: {:?}", args.publish);
    println!("  save_snapshot: {:?}", args.save_snapshot);
    println!("  wait_ready: {:?}", args.wait_ready);
    println!("  logs: {}", args.logs);
    println!("  balloon: {:?}", args.balloon);
    println!("(TODO) implement: preflight, net (rootless/privileged), disks, Firecracker API, MMDS, vsock, snapshots.");
    Ok(())
}

async fn cmd_clone(args: CloneArgs) -> Result<()> {
    println!("(stub) fcvm clone");
    println!("  from: {}", args.name);
    println!("  snapshot: {}", args.snapshot);
    println!("  mode: {:?}", args.mode);
    println!("  publish: {:?}", args.publish);
    println!("  logs: {}", args.logs);
    println!("(TODO) implement: restore from snapshot, identity patching, CoW disks, Firecracker API.");
    Ok(())
}
