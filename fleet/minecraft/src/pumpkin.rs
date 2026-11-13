//! Pumpkin (github.com/Pumpkin-MC/Pumpkin), run unmodified as the tile's server: its
//! configuration written before it starts, then the process supervised. The tile ends when
//! Pumpkin ends, so the VM fails and the agent relaunches it.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::Duration;

/// Players one server takes at once, the tile's own bots among them: Pumpkin refuses a login past
/// it. The servers take public joins (docs/fleet/minecraft.md, "Public joins").
pub const MAX_PLAYERS: u32 = 20;
/// Seconds a connection has to finish logging in before Pumpkin closes it (`login_timeout`, the
/// ejc3/Pumpkin fork's; upstream has none, so a silent connection was held for as long as its
/// client kept it open).
pub const LOGIN_TIMEOUT_SECS: u64 = 15;

#[derive(Debug, Clone)]
pub struct ServerCfg {
    pub bin: PathBuf,
    pub dir: PathBuf,
    pub port: u16,
    pub seed: String,
    pub view_distance: u8,
    pub max_players: u32,
    pub login_timeout_secs: u64,
    pub motd: String,
}

impl ServerCfg {
    /// The tile's server: the public limits above, a seed and view distance from its arguments.
    pub fn tile(bin: PathBuf, dir: PathBuf, port: u16, seed: String, view_distance: u8) -> ServerCfg {
        ServerCfg {
            bin,
            dir,
            port,
            seed,
            view_distance,
            max_players: MAX_PLAYERS,
            login_timeout_secs: LOGIN_TIMEOUT_SECS,
            motd: "fcvm Minecraft tile (Pumpkin)".into(),
        }
    }
}

/// `pumpkin.toml`: offline mode (bots and players need no account), creative so everyone can
/// build, no spawn protection, a fixed seed so every tile is the same world, a short view
/// distance, and nothing that reaches out: no telemetry, no Bedrock listener, no plugins.
/// The Java listener is the only one: the query, management and proxy services are off by
/// name, not by Pumpkin's defaults, since the servers take public joins.
pub fn config(c: &ServerCfg) -> String {
    format!(
        r#"seed = "{seed}"
default_gamemode = "Creative"
force_gamemode = true
spawn_protection = 0
use_favicon = false

[logging]
color = false

[networking.java]
address = "[::]:{port}"
encryption = false
online_mode = false
max_players = {max}
login_timeout = {login}
view_distance = {vd}
simulation_distance = {vd}
motd = "{motd}"

[networking.java.authentication]
enabled = false

[networking.java.authentication.textures]
enabled = false

[networking.bedrock]
enabled = false
online_mode = false

[networking.bedrock.authentication]
enabled = false

[networking.lan_broadcast]
enabled = false

[networking.query]
enabled = false

[networking.management]
enabled = false

[networking.proxy]
enabled = false

[telemetry]
enabled = false

[plugins]
enabled = false

[commands]
use_tty = false
"#,
        seed = c.seed.replace(['"', '\\'], ""),
        port = c.port,
        max = c.max_players,
        login = c.login_timeout_secs,
        vd = c.view_distance,
        motd = c.motd.replace(['"', '\\', '\n'], ""),
    )
}

pub fn write_config(c: &ServerCfg) -> Result<()> {
    std::fs::create_dir_all(&c.dir)?;
    std::fs::write(c.dir.join("pumpkin.toml"), config(c)).context("writing pumpkin.toml")
}

/// Start Pumpkin in its directory; its output is the tile's.
pub fn spawn(c: &ServerCfg) -> Result<tokio::process::Child> {
    write_config(c)?;
    tokio::process::Command::new(&c.bin)
        .current_dir(&c.dir)
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {}", c.bin.display()))
}

/// Wait until something accepts on `addr`.
pub async fn wait_listening(addr: &str, deadline: Duration) -> Result<()> {
    let end = tokio::time::Instant::now() + deadline;
    loop {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return Ok(());
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < end,
            "{addr} did not accept within {deadline:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_is_offline_creative_and_reaches_nowhere() {
        let c = ServerCfg {
            bin: "/x".into(),
            dir: "/d".into(),
            port: 25565,
            seed: "fc\"vm".into(),
            view_distance: 6,
            max_players: 20,
            login_timeout_secs: 15,
            motd: "mc-1\n".into(),
        };
        let t: toml::Value = toml::from_str(&config(&c)).unwrap();
        assert_eq!(t["seed"].as_str(), Some("fcvm"));
        assert_eq!(t["default_gamemode"].as_str(), Some("Creative"));
        assert_eq!(
            t["networking"]["java"]["online_mode"].as_bool(),
            Some(false)
        );
        assert_eq!(
            t["networking"]["java"]["address"].as_str(),
            Some("[::]:25565")
        );
        assert_eq!(t["networking"]["java"]["motd"].as_str(), Some("mc-1"));
        assert_eq!(t["telemetry"]["enabled"].as_bool(), Some(false));
        assert_eq!(t["networking"]["bedrock"]["enabled"].as_bool(), Some(false));
        assert_eq!(t["plugins"]["enabled"].as_bool(), Some(false));
        // The Java listener is the only one a public player can reach.
        for svc in ["bedrock", "lan_broadcast", "query", "management", "proxy"] {
            assert_eq!(
                t["networking"][svc]["enabled"].as_bool(),
                Some(false),
                "{svc}"
            );
        }
    }

    #[test]
    fn the_tile_caps_its_players_and_closes_a_login_that_does_not_finish() {
        let c = ServerCfg::tile("/x".into(), "/d".into(), 25565, "fcvm".into(), 6);
        let t: toml::Value = toml::from_str(&config(&c)).unwrap();
        let java = &t["networking"]["java"];
        // 0 would be no cap at all; the public limits need both.
        assert_eq!(java["max_players"].as_integer(), Some(20));
        assert_eq!(java["login_timeout"].as_integer(), Some(15));
    }
}
