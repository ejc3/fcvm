pub mod vsock;
pub mod http;
pub mod log;
pub mod exec;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Readiness gate specification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadinessGate {
    pub mode: ReadinessMode,
    pub config: ReadinessConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReadinessMode {
    Vsock,
    Http,
    Log,
    Exec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ReadinessConfig {
    Vsock { port: u32 },
    Http { url: String, timeout_secs: u64 },
    Log { pattern: String },
    Exec { command: String },
}

impl ReadinessGate {
    /// Parse from CLI string: "mode=vsock" or "mode=http,url=http://..."
    pub fn parse(s: &str) -> Result<Self> {
        let mut mode = None;
        let mut url = None;
        let mut pattern = None;
        let mut command = None;
        let mut port = None;

        for part in s.split(',') {
            let kv: Vec<&str> = part.splitn(2, '=').collect();
            if kv.len() != 2 {
                continue;
            }

            match kv[0] {
                "mode" => mode = Some(kv[1]),
                "url" => url = Some(kv[1].to_string()),
                "pattern" => pattern = Some(kv[1].to_string()),
                "command" => command = Some(kv[1].to_string()),
                "port" => port = Some(kv[1].parse()?),
                _ => {}
            }
        }

        let mode = mode.ok_or_else(|| anyhow::anyhow!("readiness mode required"))?;

        let (mode, config) = match mode {
            "vsock" => (
                ReadinessMode::Vsock,
                ReadinessConfig::Vsock {
                    port: port.unwrap_or(9000),
                },
            ),
            "http" => (
                ReadinessMode::Http,
                ReadinessConfig::Http {
                    url: url.ok_or_else(|| anyhow::anyhow!("url required for http mode"))?,
                    timeout_secs: 60,
                },
            ),
            "log" => (
                ReadinessMode::Log,
                ReadinessConfig::Log {
                    pattern: pattern.ok_or_else(|| anyhow::anyhow!("pattern required for log mode"))?,
                },
            ),
            "exec" => (
                ReadinessMode::Exec,
                ReadinessConfig::Exec {
                    command: command.ok_or_else(|| anyhow::anyhow!("command required for exec mode"))?,
                },
            ),
            _ => anyhow::bail!("invalid readiness mode: {}", mode),
        };

        Ok(Self { mode, config })
    }

    /// Wait for readiness
    pub async fn wait(&self) -> Result<()> {
        match (&self.mode, &self.config) {
            (ReadinessMode::Vsock, ReadinessConfig::Vsock { port }) => {
                vsock::wait_vsock(*port).await
            }
            (ReadinessMode::Http, ReadinessConfig::Http { url, timeout_secs }) => {
                http::wait_http(url, *timeout_secs).await
            }
            (ReadinessMode::Log, ReadinessConfig::Log { pattern }) => {
                log::wait_log(pattern).await
            }
            (ReadinessMode::Exec, ReadinessConfig::Exec { command }) => {
                exec::wait_exec(command).await
            }
            _ => anyhow::bail!("mismatched readiness mode and config"),
        }
    }
}
