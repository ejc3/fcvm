//! `fcvm fleet`: the GPU wall's control plane (docs/fleet/ARCHITECTURE.md, fleet/PROTOCOL.md).
//!
//! - [`coord`]: the coordinator. One task owns the fleet state and applies every event
//!   through `fleet_core::apply`; browsers, agents and timers only send it events.
//! - [`agent`]: one per GPU host. One supervisor task per slot owns that slot's fcvm
//!   child process, so a slot's VM is only ever touched by one task.
//! - [`renders`]: pages mode's host module, run on its own by `fcvm fleet renders-bench`.
//!
//! The pure parts (state, events, `apply`, tiling, wire types) are the `fleet-core` crate.

pub mod agent;
pub mod aggregate;
pub mod coord;
pub mod cpuset;
pub mod lambda_tile;
pub mod lines;
pub mod renders;
pub mod tunnel;

use crate::cli::{FleetArgs, FleetCommand};
use anyhow::Result;

pub async fn cmd_fleet(args: FleetArgs) -> Result<()> {
    match args.cmd {
        FleetCommand::Coord(a) => coord::run(*a).await,
        FleetCommand::Agent(a) => agent::run(*a).await,
        FleetCommand::Aggregate(a) => aggregate::cmd(*a).await,
        FleetCommand::PhoneWebProxy(a) => {
            agent::webproxy::spawn(a.listen).await?.await?;
            Ok(())
        }
        FleetCommand::LambdaTile(a) => lambda_tile::run(*a).await,
        FleetCommand::RendersBench(a) => renders::bench(*a).await,
        FleetCommand::RendersReap(a) => renders::reap::cmd(a).await,
    }
}

/// Unix milliseconds now.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
