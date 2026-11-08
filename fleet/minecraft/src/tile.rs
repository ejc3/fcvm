//! What a tile's parts share: the scene the agent set, the take-over, the bots' handles, the map
//! and the status the page is sent, and the times the agent reads from `/health`.

use crate::drive::Drive;
use crate::map::{Grid, Region};
use crate::tps::Tps;
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tokio::sync::broadcast;

pub fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
pub struct Cfg {
    /// Bots to run in this tile (`MCTILE_BOTS`).
    pub bots: usize,
    /// The Minecraft server's address the bots join.
    pub server: String,
    /// The map's side in blocks (`MCTILE_REGION`).
    pub region_side: u32,
    /// Bot names start with this.
    pub bot_prefix: String,
}

/// What the agent sets with `POST /scene`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Scene {
    pub tile: String,
    pub generation: u64,
    #[serde(skip)]
    pub admit_keys: Vec<[u8; 32]>,
    /// The viewer who holds the tile, and until when (unix ms).
    pub driver: Option<String>,
    pub driver_until: Option<u64>,
    pub set: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Player {
    pub name: String,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub yaw: f32,
    pub bot: bool,
    pub driven: bool,
}

/// The times `/health` reports, unix ms on the guest's clock.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Times {
    pub start: u64,
    pub server_listening: Option<u64>,
    /// The first bot's first chunk of the current session (its latest join).
    pub first_chunk: Option<u64>,
    pub bots_spawned: Option<u64>,
    pub map_ready: Option<u64>,
}

pub struct Tile {
    pub cfg: Cfg,
    pub started: Instant,
    pub scene: RwLock<Scene>,
    pub drive: Mutex<Drive>,
    /// The bots' clients, by bot number; `None` until it joined.
    pub bots: Mutex<Vec<Option<azalea::Client>>>,
    pub region: RwLock<Option<Region>>,
    pub grid: RwLock<Option<Grid>>,
    pub players: RwLock<Vec<Player>>,
    pub tps: Mutex<Tps>,
    pub times: Mutex<Times>,
    /// How many times the first bot joined: a restored tile's bots join again.
    pub sessions: AtomicU64,
    /// Chunks the first bot has received in its current session.
    pub chunks: AtomicU64,
    pub viewers: AtomicU64,
    /// The view messages every socket is sent (deltas and status), as JSON text.
    pub tx: broadcast::Sender<Arc<str>>,
    pub map_seq: AtomicU64,
    /// The last drive whose effect a status message showed: (seq, unix ms).
    pub drive_seen: Mutex<Option<(u64, u64)>>,
    pub error: Mutex<Option<String>>,
    /// The most common top blocks in the map, for `/health`.
    pub top_blocks: Mutex<Vec<(&'static str, u32)>>,
}

impl Tile {
    pub fn new(cfg: Cfg) -> Arc<Tile> {
        let (tx, _) = broadcast::channel(256);
        Arc::new(Tile {
            bots: Mutex::new(vec![None; cfg.bots]),
            cfg,
            started: Instant::now(),
            scene: RwLock::new(Scene {
                tile: "mc".into(),
                ..Default::default()
            }),
            drive: Mutex::new(Drive::default()),
            region: RwLock::new(None),
            grid: RwLock::new(None),
            players: RwLock::new(Vec::new()),
            tps: Mutex::new(Tps::default()),
            times: Mutex::new(Times {
                start: unix_ms(),
                ..Default::default()
            }),
            sessions: AtomicU64::new(0),
            chunks: AtomicU64::new(0),
            viewers: AtomicU64::new(0),
            tx,
            map_seq: AtomicU64::new(0),
            drive_seen: Mutex::new(None),
            error: Mutex::new(None),
            top_blocks: Mutex::new(Vec::new()),
        })
    }

    pub fn mono_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    pub fn send(&self, v: &serde_json::Value) {
        let _ = self.tx.send(Arc::from(v.to_string()));
    }

    /// Ready once the server listens, every bot has spawned and the map holds every column.
    pub fn ready(&self) -> bool {
        let t = self.times.lock();
        t.server_listening.is_some()
            && t.map_ready.is_some()
            && (self.cfg.bots == 0 || t.bots_spawned.is_some())
    }

    pub fn health(&self) -> serde_json::Value {
        let scene = self.scene.read().clone();
        let online = self
            .bots
            .lock()
            .iter()
            .filter(|b| b.as_ref().is_some_and(|c| c.logged_in()))
            .count();
        let grid = self.grid.read();
        serde_json::json!({
            "ready": self.ready(),
            "tile": scene.tile,
            "generation": scene.generation,
            "scene": scene.set,
            "tps": self.tps.lock().tps(),
            // A rate whose last game time is old says nothing about the server now.
            "tps_age_ms": self.tps.lock().age(self.mono_ms()),
            "bots": {"wanted": self.cfg.bots, "online": online},
            "players": self.players.read().len(),
            "sessions": self.sessions.load(Ordering::Relaxed),
            "chunks": self.chunks.load(Ordering::Relaxed),
            "map": grid.as_ref().map(|g| serde_json::json!({"known": g.known, "cells": g.cells.len()})),
            "viewers": self.viewers.load(Ordering::Relaxed),
            "driver": scene.driver,
            "times_unix_ms": self.times.lock().clone(),
            "error": self.error.lock().clone(),
            "top_blocks": self.top_blocks.lock().clone(),
        })
    }
}
