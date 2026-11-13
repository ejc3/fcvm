//! `mctile`: the program beside the Minecraft server in each tile VM of the fleet's Minecraft
//! mode (docs/fleet/minecraft.md), and the host-side bot load of its density bench.
//!
//! - `mctile tile`: start Pumpkin, join the bots, serve the map and the take-over.
//! - `mctile bots --server <addr>`: only the bots and `/health` (the bench's load, run beside a
//!   server it does not start).

mod admit;
mod bots;
mod drive;
mod map;
mod palette;
mod pumpkin;
mod server;
mod tile;
mod tps;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "mctile")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args, Clone)]
struct Common {
    /// Bots to run
    #[arg(long, env = "MCTILE_BOTS", default_value_t = 3)]
    bots: usize,
    /// The map's side in blocks, around the spawn
    #[arg(long, env = "MCTILE_REGION", default_value_t = 128)]
    region: u32,
    /// Bot names start with this
    #[arg(long, env = "MCTILE_BOT_PREFIX", default_value = "fcvm_bot")]
    bot_prefix: String,
    /// Plain HTTP (the host's)
    #[arg(long, env = "PORT", default_value_t = 8080)]
    port: u16,
}

#[derive(Subcommand)]
enum Cmd {
    Tile {
        #[command(flatten)]
        common: Common,
        #[arg(long, env = "PUMPKIN_BIN", default_value = "/usr/local/bin/pumpkin")]
        pumpkin: std::path::PathBuf,
        #[arg(long, env = "PUMPKIN_DIR", default_value = "/srv/pumpkin")]
        dir: std::path::PathBuf,
        #[arg(long, env = "MC_PORT", default_value_t = 25565)]
        mc_port: u16,
        /// The world seed: every tile is the same world. "fcvm1" spawns in a forest by a lake.
        #[arg(long, env = "MC_SEED", default_value = "fcvm1")]
        seed: String,
        #[arg(long, env = "MC_VIEW_DISTANCE", default_value_t = 6)]
        view_distance: u8,
        /// The TLS file (fleet/PROTOCOL.md, "TLS" under "Renderer")
        #[arg(long, env = "TLS_CERT_FILE")]
        tls_cert_file: Option<std::path::PathBuf>,
        #[arg(long, env = "TLS_PORT", default_value_t = 443)]
        tls_port: u16,
    },
    Bots {
        #[command(flatten)]
        common: Common,
        /// The server the bots join
        #[arg(long)]
        server: String,
    },
    /// The restore hook: tell the running tile it was restored (POST /restored on PORT).
    Restored {
        #[arg(long, env = "PORT", default_value_t = 8080)]
        port: u16,
    },
}

fn tile_cfg(c: &Common, server: String) -> tile::Cfg {
    tile::Cfg {
        bots: c.bots,
        server,
        region_side: c.region.clamp(32, 512) / 16 * 16,
        bot_prefix: c.bot_prefix.clone(),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("MCTILE_LOG")
                .unwrap_or_else(|_| "info,azalea=warn,azalea::pathfinder=error,azalea_client=warn,azalea_core=off,azalea_world=warn".into()),
        )
        .with_ansi(false)
        .init();
    match Cli::parse().cmd {
        Cmd::Tile {
            common,
            pumpkin: bin,
            dir,
            mc_port,
            seed,
            view_distance,
            tls_cert_file,
            tls_port,
        } => {
            let sc = pumpkin::ServerCfg::tile(bin, dir, mc_port, seed, view_distance);
            let t = tile::Tile::new(tile_cfg(&common, format!("127.0.0.1:{mc_port}")));
            let tls = Arc::new(server::Tls::default());
            let app = server::App {
                tile: t.clone(),
                tls: tls.clone(),
                fleet: tls_cert_file.is_some(),
            };
            let router = server::router(app.clone());
            // The plain listener (the host's) also takes the restore hook's POST /restored.
            let plain = tokio::spawn(server::serve_plain(
                ([0, 0, 0, 0, 0, 0, 0, 0], common.port).into(),
                server::local_router(app),
            ));
            if let Some(f) = tls_cert_file {
                tokio::spawn(server::watch_tls(f, tls.clone()));
                tokio::spawn(server::serve_tls(
                    ([0, 0, 0, 0, 0, 0, 0, 0], tls_port).into(),
                    router,
                    tls,
                ));
            }
            let mut child = pumpkin::spawn(&sc)?;
            let addr = format!("127.0.0.1:{mc_port}");
            tokio::select! {
                r = pumpkin::wait_listening(&addr, std::time::Duration::from_secs(300)) => r?,
                s = child.wait() => anyhow::bail!("pumpkin exited before it listened: {:?}", s?),
            }
            t.times.lock().server_listening = Some(tile::unix_ms());
            tracing::info!(addr, "the server listens; joining the bots");
            tokio::spawn(bots::control(t.clone()));
            // azalea's swarm is not Send, so it runs on this task.
            tokio::select! {
                s = child.wait() => anyhow::bail!("pumpkin exited: {:?}", s?),
                r = plain => anyhow::bail!("the HTTP listener ended: {r:?}"),
                _ = bots::run(t.clone()) => anyhow::bail!("the bots ended"),
            }
        }
        Cmd::Restored { port } => {
            // Plain HTTP/1.1 over loopback; no client library for one request.
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
            s.write_all(b"POST /restored HTTP/1.1\r\nhost: localhost\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await?;
            let mut out = String::new();
            s.read_to_string(&mut out).await?;
            anyhow::ensure!(
                out.starts_with("HTTP/1.1 200"),
                "the tile refused: {}",
                out.lines().next().unwrap_or("")
            );
            Ok(())
        }
        Cmd::Bots { common, server } => {
            let t = tile::Tile::new(tile_cfg(&common, server.clone()));
            t.times.lock().server_listening = Some(tile::unix_ms());
            let app = server::App {
                tile: t.clone(),
                tls: Arc::new(server::Tls::default()),
                fleet: false,
            };
            let plain = tokio::spawn(server::serve_plain(
                ([127, 0, 0, 1], common.port).into(),
                server::router(app),
            ));
            tokio::spawn(bots::control(t.clone()));
            tokio::select! {
                _ = bots::run(t) => anyhow::bail!("the bots ended"),
                r = plain => anyhow::bail!("the HTTP listener ended: {r:?}"),
            }
        }
    }
}
