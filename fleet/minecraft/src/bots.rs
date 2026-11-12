//! The tile's bots (azalea): they join the server, walk about the map's region and build small
//! towers of their own colour, and the first of them is the one a viewer drives when they take
//! the tile over. Their shared view of the world is what the map is read from.

use crate::drive::{self, Action, Key};
use crate::map::{Cell, Grid, Region};
use crate::palette;
use crate::tile::{Player, Tile, unix_ms};
use azalea::pathfinder::PathfinderClientExt;
use azalea::pathfinder::goals::XZGoal;
use azalea::prelude::*;
use azalea::protocol::packets::game::ClientboundGamePacket;
use azalea::swarm::prelude::*;
use azalea::{BlockPos, WalkDirection};
use rand::{Rng, SeedableRng};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// The wool each bot builds with, by bot number.
pub const COLORS: [&str; 8] = [
    "red", "blue", "yellow", "lime", "magenta", "orange", "cyan", "purple",
];

#[derive(Clone, Component, Default)]
pub struct BotState {
    tile: Option<Arc<Tile>>,
    n: usize,
}

pub fn name(tile: &Tile, n: usize) -> String {
    format!("{}{}", tile.cfg.bot_prefix, n + 1)
}

/// Join every bot; returns when the swarm stops.
pub async fn run(tile: Arc<Tile>) {
    if tile.cfg.bots == 0 {
        // Nothing to join; the tile still serves its health (the density bench's servers).
        std::future::pending::<()>().await;
    }
    let mut b = SwarmBuilder::new().set_handler(handle);
    for n in 0..tile.cfg.bots {
        b = b.add_account_with_state(
            Account::offline(&name(&tile, n)),
            BotState {
                tile: Some(tile.clone()),
                n,
            },
        );
    }
    let server = tile.cfg.server.clone();
    b.join_delay(Duration::from_millis(100))
        .reconnect_after(Duration::from_millis(100))
        .start(server.as_str())
        .await;
}

async fn handle(bot: Client, event: Event, state: BotState) -> anyhow::Result<()> {
    let Some(tile) = state.tile else {
        return Ok(());
    };
    match event {
        Event::Init => {
            tile.bots.lock()[state.n] = Some(bot.clone());
        }
        // Every join, the first and each one after a restore retired the connection.
        Event::Login => {
            tile.bots.lock()[state.n] = Some(bot.clone());
            if state.n == 0 {
                tile.sessions.fetch_add(1, Ordering::Relaxed);
                tile.chunks.store(0, Ordering::Relaxed);
                tile.times.lock().first_chunk = None;
                tile.tps.lock().clear();
            }
        }
        Event::Spawn => {
            give_wool(&bot, state.n);
            let mut t = tile.times.lock();
            let spawned = tile
                .bots
                .lock()
                .iter()
                .flatten()
                .filter(|c| c.logged_in())
                .count();
            if spawned >= tile.cfg.bots && t.bots_spawned.is_none() {
                t.bots_spawned = Some(unix_ms());
            }
        }
        Event::Packet(p) if state.n == 0 => match &*p {
            ClientboundGamePacket::SetTime(t) => {
                tile.tps.lock().sample(tile.mono_ms(), t.game_time)
            }
            ClientboundGamePacket::LevelChunkWithLight(_)
                if tile.chunks.fetch_add(1, Ordering::Relaxed) == 0 =>
            {
                tile.times.lock().first_chunk = Some(unix_ms());
            }
            _ => {}
        },
        Event::Disconnect(_) if state.n == 0 => {
            tile.drive.lock().keys.clear();
        }
        _ => {}
    }
    Ok(())
}

/// A stack of the bot's wool in its first hotbar slot (the servers run in creative mode).
fn give_wool(bot: &Client, n: usize) {
    use azalea::inventory::ItemStack;
    use azalea::protocol::packets::game::ServerboundSetCreativeModeSlot;
    use azalea::registry::builtin::ItemKind;
    let item = match COLORS[n % COLORS.len()] {
        "red" => ItemKind::RedWool,
        "blue" => ItemKind::BlueWool,
        "yellow" => ItemKind::YellowWool,
        "lime" => ItemKind::LimeWool,
        "magenta" => ItemKind::MagentaWool,
        "orange" => ItemKind::OrangeWool,
        "cyan" => ItemKind::CyanWool,
        _ => ItemKind::PurpleWool,
    };
    bot.write_packet(ServerboundSetCreativeModeSlot {
        slot_num: 36,
        item_stack: ItemStack::new(item, 64),
    });
    bot.set_selected_hotbar_slot(0);
}

/// What a bot that nobody drives is doing.
#[derive(Debug, Clone)]
enum Pilot {
    /// Walking to a column.
    Walk {
        x: i32,
        z: i32,
        until: u64,
    },
    /// Building a tower beside where it stands: blocks placed so far.
    Build {
        x: i32,
        z: i32,
        placed: u32,
        next: u64,
    },
    Idle {
        until: u64,
    },
}

/// What the bots' choices are reseeded on: a new session (the bots joined again) or a new scene
/// (the agent posted this tile's own name and generation, as it does to a fork's copy).
fn reseed_key(tile: &Tile) -> (u64, u64, String) {
    let scene = tile.scene.read();
    (
        tile.sessions.load(Ordering::Relaxed),
        scene.generation,
        scene.tile.clone(),
    )
}

fn seed(tile: &Tile) -> u64 {
    // A restored tile runs the same program from the same memory as its source; the scene's
    // name and generation and the guest's random pool make each copy choose its own way.
    let s = tile.scene.read();
    let mut b = [0u8; 8];
    let _ = getrandom::fill(&mut b);
    u64::from_le_bytes(b)
        ^ s.generation.rotate_left(17)
        ^ s.tile
            .bytes()
            .fold(0u64, |h, c| h.wrapping_mul(31).wrapping_add(c as u64))
        ^ unix_ms()
}

/// The control loop: the driven bot follows the take-over, the others their pilots, and the map
/// and status go to the page. Runs until the process ends.
pub async fn control(tile: Arc<Tile>) {
    let mut pilots: Vec<Pilot> = vec![Pilot::Idle { until: 0 }; tile.cfg.bots];
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed(&tile));
    // Choices are reseeded on each new session and each new scene: a restore keeps the bots
    // connected (their loopback connections survive it), so the scene the agent posts to the
    // copy, with its own name and generation, is what tells a fork from its source.
    let mut session = (0, 0, String::new());
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut n_tick: u64 = 0;
    loop {
        tick.tick().await;
        n_tick += 1;
        let now = unix_ms();
        let s = reseed_key(&tile);
        if s != session {
            session = s;
            rng = rand::rngs::StdRng::seed_from_u64(seed(&tile));
            pilots
                .iter_mut()
                .for_each(|p| *p = Pilot::Idle { until: now + 500 });
        }
        let bots: Vec<Option<Client>> = tile.bots.lock().clone();
        let Some(first) = bots.iter().flatten().find(|c| c.logged_in()).cloned() else {
            continue;
        };
        let region = {
            let mut r = tile.region.write();
            if r.is_none()
                && let Ok(p) = first.position()
            {
                *r = Some(Region::around(
                    p.x.floor() as i32,
                    p.z.floor() as i32,
                    tile.cfg.region_side,
                ));
            }
            *r
        };
        let Some(region) = region else { continue };
        for (n, bot) in bots.iter().enumerate() {
            let Some(bot) = bot.as_ref().filter(|b| b.logged_in()) else {
                continue;
            };
            let driven = n == 0 && tile.drive.lock().driver.is_some();
            if driven {
                drive_bot(&tile, bot, now);
                pilots[n] = Pilot::Idle { until: now + 2_000 };
            } else {
                fly(&tile, bot, &mut pilots[n], region, &mut rng, now);
            }
        }
        if n_tick.is_multiple_of(2) {
            map_tick(&tile, &first, region);
        }
        status_tick(&tile, &bots, now);
    }
}

fn drive_bot(tile: &Tile, bot: &Client, now: u64) {
    let (keys, yaw, pitch, actions) = {
        let mut d = tile.drive.lock();
        let a = std::mem::take(&mut d.actions);
        (d.held(now).to_vec(), d.yaw, d.pitch, a)
    };
    let _ = bot.set_direction(yaw, pitch);
    let (f, s) = drive::axes(&keys);
    let dir = match (f, s) {
        (1, 0) => WalkDirection::Forward,
        (-1, 0) => WalkDirection::Backward,
        (0, 1) => WalkDirection::Right,
        (0, -1) => WalkDirection::Left,
        (1, 1) => WalkDirection::ForwardRight,
        (1, -1) => WalkDirection::ForwardLeft,
        (-1, 1) => WalkDirection::BackwardRight,
        (-1, -1) => WalkDirection::BackwardLeft,
        _ => WalkDirection::None,
    };
    bot.stop_pathfinding();
    if keys.contains(&Key::Sprint) && f == 1 {
        bot.sprint(match s {
            1 => azalea::SprintDirection::ForwardRight,
            -1 => azalea::SprintDirection::ForwardLeft,
            _ => azalea::SprintDirection::Forward,
        });
    } else {
        bot.walk(dir);
    }
    let _ = bot.set_jumping(keys.contains(&Key::Jump));
    for (_, a) in actions {
        match a {
            Action::Place => bot.start_use_item(),
            Action::Dig => {
                if let Ok(azalea::core::hit_result::HitResult::Block(b)) = bot.hit_result()
                    && !b.miss
                {
                    bot.start_mining(b.block_pos);
                }
            }
        }
    }
}

/// The highest non-air block of a column, from the world the bots share.
fn top(world: &azalea::world::World, x: i32, z: i32) -> Option<(i32, azalea::block::BlockState)> {
    use azalea::core::heightmap_kind::HeightmapKind;
    use azalea::core::position::{ChunkBlockPos, ChunkPos};
    let cp = ChunkPos::new(x.div_euclid(16), z.div_euclid(16));
    let chunk = world.chunks.get(&cp)?;
    let chunk = chunk.read();
    let (lx, lz) = (x.rem_euclid(16) as u8, z.rem_euclid(16) as u8);
    let min_y = world.chunks.min_y();
    let hm = chunk
        .heightmaps
        .get(&HeightmapKind::WorldSurface)
        .or_else(|| chunk.heightmaps.get(&HeightmapKind::MotionBlocking))?;
    let mut y = hm.get_first_available(lx, lz) - 1;
    while y >= min_y {
        let s = chunk.get_block_state(&ChunkBlockPos::new(lx, y, lz), min_y)?;
        if !s.is_air() {
            return Some((y, s));
        }
        y -= 1;
    }
    None
}

fn block_name(s: azalea::block::BlockState) -> &'static str {
    let k: azalea::registry::builtin::BlockKind = s.into();
    k.to_str()
}

fn fly(tile: &Tile, bot: &Client, p: &mut Pilot, region: Region, rng: &mut impl Rng, now: u64) {
    // A quarter of the region at most, so the range is never empty (the side is at least 32).
    let margin = (region.w.min(region.h) as i32 / 4).min(12);
    match p.clone() {
        Pilot::Idle { until } if now >= until => {
            let x = rng.random_range(region.x0 + margin..region.x0 + region.w as i32 - margin);
            let z = rng.random_range(region.z0 + margin..region.z0 + region.h as i32 - margin);
            bot.start_goto(XZGoal { x, z });
            *p = Pilot::Walk {
                x,
                z,
                until: now + 25_000,
            };
        }
        Pilot::Idle { .. } => {}
        Pilot::Walk { x, z, until } => {
            let near = bot.position().is_ok_and(|q| {
                (q.x - x as f64 - 0.5).abs() < 2.0 && (q.z - z as f64 - 0.5).abs() < 2.0
            });
            if near || now >= until || bot.is_goto_target_reached() {
                bot.stop_pathfinding();
                let Ok(q) = bot.position() else { return };
                *p = Pilot::Build {
                    x: q.x.floor() as i32 + 1,
                    z: q.z.floor() as i32,
                    placed: 0,
                    next: now + 300,
                };
            }
        }
        Pilot::Build { x, z, placed, next } => {
            if now < next {
                return;
            }
            if placed >= 3 + (rng.random::<u8>() % 3) as u32 {
                *p = Pilot::Idle {
                    until: now + rng.random_range(500..3_000),
                };
                return;
            }
            let Ok(world) = bot.world() else { return };
            let Some((y, _)) = top(&world.read(), x, z) else {
                *p = Pilot::Idle { until: now + 1_000 };
                return;
            };
            bot.look_at(BlockPos::new(x, y, z).center());
            bot.block_interact(BlockPos::new(x, y, z));
            *p = Pilot::Build {
                x,
                z,
                placed: placed + 1,
                next: now + 400,
            };
            let _ = tile;
        }
    }
}

fn map_tick(tile: &Tile, bot: &Client, region: Region) {
    let Ok(world) = bot.world() else { return };
    let world = world.read();
    let mut g = Grid::new(region);
    let mut names: std::collections::BTreeMap<&'static str, u32> = Default::default();
    for j in 0..region.h as i32 {
        for i in 0..region.w as i32 {
            if let Some((y, s)) = top(&world, region.x0 + i, region.z0 + j) {
                let name = block_name(s);
                *names.entry(name).or_default() += 1;
                g.cells[(j * region.w as i32 + i) as usize] = Cell::new(palette::material(name), y);
                g.known += 1;
            }
        }
    }
    drop(world);
    let mut top_blocks: Vec<(&'static str, u32)> = names.into_iter().collect();
    top_blocks.sort_by_key(|b| std::cmp::Reverse(b.1));
    top_blocks.truncate(16);
    *tile.top_blocks.lock() = top_blocks;
    let mut cur = tile.grid.write();
    let seq = tile.map_seq.fetch_add(1, Ordering::Relaxed) + 1;
    match cur.as_ref() {
        Some(old) if old.region == g.region => {
            let d = old.delta(&g);
            if !d.is_empty() {
                tile.send(&serde_json::json!({"type": "delta", "seq": seq, "cells": d}));
            }
        }
        _ => tile.send(
            &serde_json::json!({"type": "map", "seq": seq, "region": region, "cells": g.encode()}),
        ),
    }
    if g.known == g.cells.len() && tile.times.lock().map_ready.is_none() {
        tile.times.lock().map_ready = Some(unix_ms());
    }
    *cur = Some(g);
}

fn status_tick(tile: &Tile, bots: &[Option<Client>], now: u64) {
    let names: Vec<String> = (0..tile.cfg.bots).map(|n| name(tile, n)).collect();
    let mut players = Vec::new();
    if let Some(Some(b)) = bots
        .iter()
        .find(|b| b.as_ref().is_some_and(|c| c.logged_in()))
    {
        use azalea::ecs::prelude::With;
        use azalea::entity::{LookDirection, Position, metadata::Player as PlayerMarker};
        if let Ok(list) = b.tab_list() {
            let mut ecs = b.ecs.write();
            let mut q = ecs.query_filtered::<(&azalea::entity::EntityUuid, &Position, &LookDirection), With<PlayerMarker>>();
            let mut seen = std::collections::HashSet::new();
            for (uuid, pos, look) in q.iter(&ecs) {
                if !seen.insert(**uuid) {
                    continue;
                }
                let Some(info) = list.get(&**uuid) else {
                    continue;
                };
                let name = info.profile.name.clone();
                let bot = names.contains(&name);
                players.push(Player {
                    driven: bot && name == names[0] && tile.drive.lock().driver.is_some(),
                    bot,
                    name,
                    x: pos.x,
                    y: pos.y,
                    z: pos.z,
                    yaw: look.y_rot(),
                });
            }
        }
    }
    players.sort_by(|a, b| a.name.cmp(&b.name));
    // The take-over's first visible change: the driven bot moved since the drive arrived.
    let drive = {
        let d = tile.drive.lock();
        (d.driver.is_some() && d.seq > 0).then_some((d.seq, d.received_at))
    };
    if let Some((seq, at)) = drive {
        let old = tile.players.read().iter().find(|p| p.driven).cloned();
        let new = players.iter().find(|p| p.driven);
        if let (Some(o), Some(n)) = (old, new) {
            let moved = (o.x - n.x).abs() > 0.01
                || (o.z - n.z).abs() > 0.01
                || (o.y - n.y).abs() > 0.01
                || (o.yaw - n.yaw).abs() > 0.5;
            let mut seen = tile.drive_seen.lock();
            if moved && seen.is_none_or(|(s, _)| s != seq) && now >= at {
                *seen = Some((seq, now));
            }
        }
    }
    *tile.players.write() = players.clone();
    let tps = tile.tps.lock().tps();
    let seen = *tile.drive_seen.lock();
    tile.send(&serde_json::json!({
        "type": "status", "t": now, "tps": tps, "players": players,
        "drive": seen.map(|(seq, at)| serde_json::json!({"seq": seq, "moved_at": at})),
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A restore keeps the bots connected, so the session count stays; the copy's own scene is
    /// what changes the bots' choices, so a fork and its source do not walk the same paths.
    #[test]
    fn a_new_scene_reseeds_the_bots_as_a_new_session_does() {
        let tile = Tile::new(crate::tile::Cfg {
            bots: 0,
            server: "127.0.0.1:1".into(),
            region_side: 16,
            bot_prefix: "b".into(),
        });
        let k0 = reseed_key(&tile);
        assert_eq!(reseed_key(&tile), k0);
        {
            let mut s = tile.scene.write();
            s.tile = "mc-2".into();
            s.generation = 7;
        }
        let k1 = reseed_key(&tile);
        assert_ne!(k1, k0, "the copy's scene");
        tile.sessions.fetch_add(1, Ordering::Relaxed);
        assert_ne!(reseed_key(&tile), k1, "a new session");
    }
}
