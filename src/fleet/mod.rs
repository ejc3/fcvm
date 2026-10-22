//! `fcvm fleet`: the GPU wall's control plane (docs/fleet/ARCHITECTURE.md, fleet/PROTOCOL.md).
//!
//! - [`coord`]: the coordinator. One task owns the fleet state and applies every event
//!   through `fleet_core::apply`; browsers, agents and timers only send it events.
//! - [`agent`]: one per GPU host. One supervisor task per slot owns that slot's fcvm
//!   child process, so a slot's VM is only ever touched by one task.
//!
//! The pure parts (state, events, `apply`, tiling, wire types) are the `fleet-core` crate.

pub mod agent;
pub mod coord;
pub mod lines;

use crate::cli::{FleetArgs, FleetCommand};
use anyhow::Result;

pub async fn cmd_fleet(args: FleetArgs) -> Result<()> {
    match args.cmd {
        FleetCommand::Coord(a) => coord::run(a).await,
        FleetCommand::Agent(a) => agent::run(a).await,
    }
}

/// Unix milliseconds now.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
