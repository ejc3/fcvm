//! The tile's bots (azalea): they join the server, walk about the dry land of the map's region
//! and build walls, the outlines of houses and towers of their own colour (`plan`), and the
//! first of them is the one a viewer drives when they take the tile over. Their shared view of
//! the world is what the map is read from.

use crate::drive::{self, Action, Key};
use crate::map::{Cell, Grid, Region};
use crate::palette;
use crate::plan::{self, Shape, Step};
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
    Walk { x: i32, z: i32, until: u64 },
    /// In the water: swimming straight for a dry column (up, and forward) until it stands on
    /// land or the time runs out.
    Swim { x: i32, z: i32, until: u64 },
    /// Building: the columns left to raise, the one it is on, and its progress there.
    Build {
        steps: Vec<Step>,
        at: usize,
        /// Walking to the column's standing place until this time; 0 when not yet sent there.
        walk_until: u64,
        /// Blocks tried on this column.
        tries: u32,
        next: u64,
    },
    /// Standing until `until`. `near`: the next walk is a short one (a bot just restored or
    /// forked builds at once, so a copy and its source differ within seconds).
    Idle { until: u64, near: bool },
}

/// How far a walk goes: across the map, or a few columns for a bot that has just been reseeded.
const WALK_REACH: i32 = 28;
const NEAR_REACH: i32 = 6;
/// A bot places a block on a column no further than this from its eyes (the server refuses a
/// block out of reach).
const REACH: f64 = 4.0;

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
    let mut pilots: Vec<Pilot> = vec![
        Pilot::Idle {
            until: 0,
            near: true
        };
        tile.cfg.bots
    ];
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
            pilots.iter_mut().for_each(|p| {
                *p = Pilot::Idle {
                    until: now + 300,
                    near: true,
                }
            });
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
                pilots[n] = Pilot::Idle {
                    until: now + 2_000,
                    near: false,
                };
            } else {
                fly(&tile, bot, &mut pilots[n], region, &mut rng, now);
            }
        }
        if n_tick.is_multiple_of(2) {
            map_tick(&tile, &first, region, n_tick / 2);
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

/// How far under a column's top block its ground is looked for: a canopy stands this far over
/// the forest floor at most, and deeper water than this is drawn this deep.
const GROUND_DEPTH: i32 = 48;

/// A column from the world the bots share: its top block (the highest that is not air) and its
/// ground (the highest that is not leaves, a plant or water: `palette::is_cover`) as the map
/// holds them, with the top block's name and y. `prev` is what the map held for the column:
/// while its top block is the same, its ground is taken from there and not read again.
fn column(
    world: &azalea::world::World,
    x: i32,
    z: i32,
    prev: Option<(Cell, Cell)>,
) -> Option<(Cell, Cell, &'static str, i32)> {
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
    let (name, material) = loop {
        if y < min_y {
            return None;
        }
        let s = chunk.get_block_state(&ChunkBlockPos::new(lx, y, lz), min_y)?;
        if !s.is_air() {
            let name = block_name(s);
            break (name, palette::material(name));
        }
        y -= 1;
    };
    let top = Cell::new(material, y);
    if !palette::is_cover(material) {
        return Some((top, top, name, y));
    }
    if let Some((was, ground)) = prev
        && was == top
    {
        return Some((top, ground, name, y));
    }
    // Under leaves, a plant or water: down to the first block that is none of them. A column
    // covered all the way down has the lowest block read as its ground.
    let mut ground = top;
    let mut below = y - 1;
    while below >= min_y && below >= y - GROUND_DEPTH {
        let Some(s) = chunk.get_block_state(&ChunkBlockPos::new(lx, below, lz), min_y) else {
            break;
        };
        if !s.is_air() {
            let m = palette::material(block_name(s));
            ground = Cell::new(m, below);
            if !palette::is_cover(m) {
                break;
            }
        }
        below -= 1;
    }
    Some((top, ground, name, y))
}

/// The y of a column's highest non-air block.
fn top_y(world: &azalea::world::World, x: i32, z: i32) -> Option<i32> {
    column(world, x, z, None).map(|c| c.3)
}

fn block_name(s: azalea::block::BlockState) -> &'static str {
    let k: azalea::registry::builtin::BlockKind = s.into();
    k.to_str()
}

/// A build carried on from its column `at`, not before `next`.
fn build_from(steps: &[Step], at: usize, next: u64) -> Pilot {
    Pilot::Build {
        steps: steps.to_vec(),
        at,
        walk_until: 0,
        tries: 0,
        next,
    }
}

/// The column a bot stands in.
fn column_of(bot: &Client) -> Option<(i32, i32)> {
    let p = bot.position().ok()?;
    Some((p.x.floor() as i32, p.z.floor() as i32))
}

fn near(bot: &Client, x: i32, z: i32, within: f64) -> bool {
    bot.position().is_ok_and(|q| {
        (q.x - x as f64 - 0.5).abs() < within && (q.z - z as f64 - 0.5).abs() < within
    })
}

/// A bot that found no dry column in reach looks again this long after.
const MISS_WAIT_MS: u64 = 1_000;

/// What a bot does after it found no dry column in reach: nothing, for [`MISS_WAIT_MS`].
fn after_miss(now: u64, near: bool) -> Pilot {
    Pilot::Idle {
        until: now + MISS_WAIT_MS,
        near,
    }
}

fn fly(tile: &Tile, bot: &Client, p: &mut Pilot, region: Region, rng: &mut impl Rng, now: u64) {
    let _ = region;
    match p.clone() {
        Pilot::Idle { until, near: short } if now >= until => {
            let Some((x, z)) = column_of(bot) else { return };
            let grid = tile.grid.read();
            let Some(grid) = grid.as_ref() else { return };
            // A bot in the water leaves it before anything else: half the time it swims
            // straight for the nearest shore, half the time the pathfinder takes it there (one
            // gets it out where the other is stuck). The shore is looked for once.
            let wet = plan::in_water(grid, x, z);
            let shore = if wet {
                plan::nearest_land(grid, x, z, 64)
            } else {
                None
            };
            if let Some((tx, tz)) = shore
                && rng.random::<bool>()
            {
                bot.stop_pathfinding();
                *p = Pilot::Swim {
                    x: tx,
                    z: tz,
                    until: now + 8_000,
                };
                return;
            }
            let target = if wet {
                shore
            } else {
                let reach = if short { NEAR_REACH } else { WALK_REACH };
                let picks: Vec<(i32, i32)> = (0..24)
                    .map(|_| {
                        (
                            rng.random_range(-reach..=reach),
                            rng.random_range(-reach..=reach),
                        )
                    })
                    .collect();
                plan::walk_target(grid, x, z, reach, &picks)
            };
            match target {
                Some((tx, tz)) if (tx, tz) != (x, z) => {
                    bot.start_goto(XZGoal { x: tx, z: tz });
                    *p = Pilot::Walk {
                        x: tx,
                        z: tz,
                        until: now + 25_000,
                    };
                }
                // Already there: build where it stands.
                Some(_) => *p = Pilot::Walk { x, z, until: now },
                // Nothing dry in reach (the map still loading, a region of water, a driven bot
                // far outside the map): look again in a while, not at the next tick.
                None => *p = after_miss(now, short),
            }
        }
        Pilot::Idle { .. } => {}
        Pilot::Swim { x, z, until } => {
            let Some((bx, bz)) = column_of(bot) else {
                return;
            };
            let dry = tile
                .grid
                .read()
                .as_ref()
                .is_some_and(|g| plan::land(g, bx, bz).is_some());
            if dry || now >= until {
                bot.walk(WalkDirection::None);
                let _ = bot.set_jumping(false);
                *p = Pilot::Idle {
                    until: now,
                    near: true,
                };
                return;
            }
            // Head for the shore at eye level; holding jump in water is swimming up.
            let eye = bot.position().map_or(64, |q| q.y.floor() as i32 + 1);
            bot.look_at(BlockPos::new(x, eye, z).center());
            bot.walk(WalkDirection::Forward);
            let _ = bot.set_jumping(true);
        }
        Pilot::Walk { x, z, until } => {
            if !(near(bot, x, z, 2.0) || now >= until || bot.is_goto_target_reached()) {
                return;
            }
            bot.stop_pathfinding();
            let Some((bx, bz)) = column_of(bot) else {
                return;
            };
            let grid = tile.grid.read();
            let Some(grid) = grid.as_ref() else { return };
            if plan::land(grid, bx, bz).is_none() {
                // The walk ended in the water or on a tree: go on to dry ground at once.
                *p = Pilot::Idle {
                    until: now,
                    near: false,
                };
                return;
            }
            // Mostly walls and houses; the first shape that has a column to raise here.
            let dir = rng.random_range(0..4usize);
            let first = match rng.random_range(0..10u32) {
                0..=3 => Shape::Wall,
                4..=7 => Shape::House,
                _ => Shape::Tower,
            };
            let steps = [first, Shape::Wall, Shape::Tower]
                .into_iter()
                .map(|shape| {
                    let (len, rise) = match shape {
                        Shape::Wall => (rng.random_range(4..=7), 2),
                        Shape::House => (rng.random_range(4..=5), 2),
                        Shape::Tower => (1, plan::MAX_RISE),
                    };
                    plan::plan(shape, grid, bx, bz, dir, len, rise)
                })
                .find(|s| !s.is_empty());
            *p = match steps {
                Some(steps) => Pilot::Build {
                    steps,
                    at: 0,
                    walk_until: 0,
                    tries: 0,
                    next: now + 300,
                },
                None => Pilot::Idle {
                    until: now + 500,
                    near: false,
                },
            };
        }
        Pilot::Build {
            steps,
            at,
            walk_until,
            tries,
            next,
        } => {
            let Some(step) = steps.get(at).copied() else {
                *p = Pilot::Idle {
                    until: now + rng.random_range(500..2_500),
                    near: false,
                };
                return;
            };
            // Walk to the column's standing place first: straight there, a step or two over
            // level ground. The pathfinder is for the long walk: a search for every column
            // (and a block every 350 ms) took 7% more CPU at 256 servers, and the search shares
            // the guest's one CPU with the server.
            if !near(bot, step.stand.0, step.stand.1, 0.4) {
                if walk_until != 0 && now >= walk_until {
                    // It could not get there: leave this column out.
                    bot.walk(WalkDirection::None);
                    let _ = bot.set_jumping(false);
                    *p = build_from(&steps, at + 1, now);
                    return;
                }
                let Ok(q) = bot.position() else { return };
                let feet = q.y.floor() as i32;
                // Jump when the place is higher than where it stands.
                let up = tile.grid.read().as_ref().is_some_and(|g| {
                    plan::land(g, step.stand.0, step.stand.1).is_some_and(|y| y + 1 > feet)
                });
                bot.look_at(BlockPos::new(step.stand.0, feet + 1, step.stand.1).center());
                bot.walk(WalkDirection::Forward);
                let _ = bot.set_jumping(up);
                if walk_until == 0 {
                    *p = Pilot::Build {
                        steps,
                        at,
                        walk_until: now + 4_000,
                        tries,
                        next,
                    };
                }
                return;
            }
            if walk_until != 0 {
                bot.walk(WalkDirection::None);
                let _ = bot.set_jumping(false);
            }
            if now < next {
                return;
            }
            let Ok(world) = bot.world() else { return };
            let Some(y) = top_y(&world.read(), step.x, step.z) else {
                *p = build_from(&steps, at + 1, now);
                return;
            };
            // Raised to its height, or more blocks tried than it needs: the next column.
            if y >= step.top || tries > (plan::MAX_RISE as u32) + 2 {
                *p = build_from(&steps, at + 1, now + 300);
                return;
            }
            let block = BlockPos::new(step.x, y, step.z);
            let in_reach = bot
                .eye_position()
                .is_ok_and(|e| e.distance_to(block.center()) <= REACH);
            if in_reach {
                // Facing the column and looking far over it, so that the bot is not looking at
                // the block: the click is then on the block's top face, and the new block goes
                // on top of it (looking at its side would put the block beside it).
                bot.look_at(BlockPos::new(step.x, y + 40, step.z).center());
                bot.block_interact(block);
            }
            *p = Pilot::Build {
                steps,
                at,
                walk_until: 0,
                tries: tries + 1,
                next: now + 600,
            };
        }
    }
}

/// Every this many map reads, a row's ground is read again from the world whatever its top
/// blocks are: a block placed under a canopy or a lake changes the ground and not the top.
const GROUND_REFRESH: u64 = 16;

fn map_tick(tile: &Tile, bot: &Client, region: Region, tick: u64) {
    let Ok(world) = bot.world() else { return };
    let world = world.read();
    let mut g = Grid::new(region);
    let mut names: std::collections::BTreeMap<&'static str, u32> = Default::default();
    // What the map held: a column whose top block has not changed keeps its ground.
    let prev = tile.grid.read().clone().filter(|p| p.region == region);
    for j in 0..region.h as i32 {
        let fresh = j as u64 % GROUND_REFRESH == tick % GROUND_REFRESH;
        for i in 0..region.w as i32 {
            let at = (j * region.w as i32 + i) as usize;
            let was = prev
                .as_ref()
                .filter(|_| !fresh)
                .map(|p| (p.cells[at], p.ground[at]));
            if let Some((top, ground, name, _)) = column(&world, region.x0 + i, region.z0 + j, was)
            {
                *names.entry(name).or_default() += 1;
                g.cells[at] = top;
                g.ground[at] = ground;
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
            if let Some(d) = old.delta_message(&g, seq) {
                tile.send(&d);
            }
        }
        _ => tile.send(&g.map_message(seq)),
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

    /// A bot with nothing dry in reach (the map loading, a region of water, a driven bot far
    /// outside the map) looked again at the very next tick, and each look was a whole search.
    #[test]
    fn a_bot_that_finds_no_dry_column_waits_before_it_looks_again() {
        for near in [true, false] {
            match after_miss(5_000, near) {
                Pilot::Idle { until, near: n } => {
                    assert_eq!((until, n), (5_000 + MISS_WAIT_MS, near));
                }
                other => panic!("{other:?}"),
            }
        }
        assert!(MISS_WAIT_MS >= 1_000);
        // and a tick's decision looks for the shore once, and leaves the miss to that wait
        let src = include_str!("bots.rs");
        let fly = &src[src.find("fn fly(").unwrap()..src.find("fn map_tick(").unwrap()];
        assert_eq!(
            fly.matches("plan::nearest_land(").count(),
            1,
            "one search a tick"
        );
        assert!(fly.contains("None => *p = after_miss(now, short),"));
    }

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
