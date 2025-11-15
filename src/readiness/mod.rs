pub mod http;

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
    Http,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ReadinessConfig {
    Http { url: String, timeout_secs: u64 },
}

impl ReadinessGate {
    /// Parse from CLI string: "mode=http,url=http://..."
    pub fn parse(s: &str) -> Result<Self> {
        let mut mode = None;
        let mut url = None;

        for part in s.split(',') {
            let kv: Vec<&str> = part.splitn(2, '=').collect();
            if kv.len() != 2 {
                continue;
            }

            match kv[0] {
                "mode" => mode = Some(kv[1]),
                "url" => url = Some(kv[1].to_string()),
                _ => {}
            }
        }

        let mode = mode.ok_or_else(|| anyhow::anyhow!("readiness mode required"))?;

        let (mode, config) = match mode {
            "http" => (
                ReadinessMode::Http,
                ReadinessConfig::Http {
                    url: url.ok_or_else(|| anyhow::anyhow!("url required for http mode"))?,
                    timeout_secs: 60,
                },
            ),
            _ => anyhow::bail!(
                "invalid readiness mode: {} (only 'http' is supported)",
                mode
            ),
        };

        Ok(Self { mode, config })
    }

    /// Wait for readiness
    pub async fn wait(&self) -> Result<()> {
        match (&self.mode, &self.config) {
            (ReadinessMode::Http, ReadinessConfig::Http { url, timeout_secs }) => {
                http::wait_http(url, *timeout_secs).await
            }
        }
    }
}
