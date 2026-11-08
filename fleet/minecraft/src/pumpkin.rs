//! Pumpkin (github.com/Pumpkin-MC/Pumpkin), run unmodified as the tile's server: its
//! configuration written before it starts, then the process supervised. The tile ends when
//! Pumpkin ends, so the VM fails and the agent relaunches it.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ServerCfg {
    pub bin: PathBuf,
    pub dir: PathBuf,
    pub port: u16,
    pub seed: String,
    pub view_distance: u8,
    pub max_players: u32,
    pub motd: String,
}

/// `pumpkin.toml`: offline mode (bots and players need no account), creative so everyone can
/// build, no spawn protection, a fixed seed so every tile is the same world, a short view
/// distance, and nothing that reaches out: no telemetry, no Bedrock listener, no plugins.
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
    }
}
