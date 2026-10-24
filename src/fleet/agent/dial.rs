//! `fcvm fleet agent --dial <wss url>`: the agent dials the coordinator and speaks the same
//! messages as over stdio, one JSON message per WebSocket text frame.
//!
//! The connection can drop (a coordinator restart, a network blip) while the host's VMs keep
//! running: the agent itself never stops. Each new connection starts with the hello and
//! then the latest of every message that describes current state (each slot's report, the
//! aggregator, a draining notice, quarantines), so a coordinator that lost the host learns
//! where every slot is without restarting a VM. Reconnects back off from 1 s to 30 s.

use super::{serve, Agent};
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

pub struct DialCfg {
    pub url: String,
    /// Sent with every connection, e.g. Cloudflare Access's service token
    /// (`CF-Access-Client-Id`, `CF-Access-Client-Secret`). Never logged.
    pub headers: Vec<(String, String)>,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
}

/// What a new connection is told first: the hello, then the latest state messages.
#[derive(Default)]
struct Replay {
    hello: Option<String>,
    keyed: BTreeMap<String, String>,
}

impl Replay {
    fn note(&mut self, line: &str) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            return;
        };
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let key = match s("type").as_str() {
            "hello" => {
                self.hello = Some(line.to_string());
                return;
            }
            "slot" => format!("slot/{}", s("id")),
            "aggregate" => "aggregate".into(),
            "draining" => "draining".into(),
            "quarantine" => format!("quarantine/{}", s("sbdf")),
            // Health readings and scrub outcomes are events, not state.
            _ => return,
        };
        self.keyed.insert(key, line.to_string());
    }

    /// A coordinator frame: an `assign` names every slot the host still runs. A slot that
    /// left it is torn down here, so its last report and its quarantine are no longer
    /// true: a quarantined GPU leaves the assignment, and comes back to it only when an
    /// operator releases it. Replaying either would undo that (a released GPU quarantined
    /// again, a removed slot reported running).
    fn assigned(&mut self, frame: &str) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(frame) else {
            return;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("assign") {
            return;
        }
        let ids: std::collections::BTreeSet<&str> = v["slots"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| s.get("id").and_then(|i| i.as_str()))
            .collect();
        self.keyed.retain(|key, line| {
            if let Some(id) = key.strip_prefix("slot/") {
                return ids.contains(id);
            }
            if key.starts_with("quarantine/") {
                let slot = serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .and_then(|q| q.get("slot").and_then(|s| s.as_str()).map(str::to_string));
                return slot.is_some_and(|s| ids.contains(s.as_str()));
            }
            true
        });
    }

    fn lines(&self) -> Vec<String> {
        self.hello
            .iter()
            .chain(self.keyed.values())
            .cloned()
            .collect()
    }
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(cfg: &DialCfg) -> Result<Ws> {
    let mut req = cfg.url.as_str().into_client_request()?;
    for (k, v) in &cfg.headers {
        req.headers_mut().insert(
            axum::http::HeaderName::from_bytes(k.as_bytes())?,
            v.parse().context("a header value that is not ASCII")?,
        );
    }
    let (ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .with_context(|| format!("dialing {}", cfg.url))?;
    Ok(ws)
}

pub async fn run(agent: Agent, cfg: DialCfg) -> Result<()> {
    let (mut to_agent, agent_in) = tokio::io::duplex(1 << 20);
    let (agent_out, from_agent) = tokio::io::duplex(1 << 20);
    let mut serving = tokio::spawn(serve(agent, agent_in, agent_out));
    let (lines_tx, mut lines) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut r = tokio::io::BufReader::new(from_agent).lines();
        while let Ok(Some(l)) = r.next_line().await {
            if lines_tx.send(l).is_err() {
                return;
            }
        }
    });
    let mut replay = Replay::default();
    let mut backoff = cfg.backoff_min;
    loop {
        // Connect, noting the agent's messages meanwhile so the replay is current. The
        // attempt is pinned: a message arriving must not drop a handshake half done.
        let ws = loop {
            let attempt = connect(&cfg);
            tokio::pin!(attempt);
            let r = loop {
                tokio::select! {
                    r = &mut attempt => break r,
                    l = lines.recv() => match l {
                        Some(l) => replay.note(&l),
                        None => return serving.await?,
                    },
                    r = &mut serving => return r?,
                }
            };
            match r {
                Ok(ws) => break ws,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), retry_in = ?backoff, "cannot reach the coordinator");
                    let wait = tokio::time::sleep(backoff);
                    tokio::pin!(wait);
                    loop {
                        tokio::select! {
                            _ = &mut wait => break,
                            l = lines.recv() => match l {
                                Some(l) => replay.note(&l),
                                None => return serving.await?,
                            },
                            r = &mut serving => return r?,
                        }
                    }
                    backoff = (backoff * 2).min(cfg.backoff_max);
                }
            }
        };
        tracing::info!(url = %cfg.url, "connected to the coordinator");
        let connected = Instant::now();
        let (mut sink, mut stream) = ws.split();
        let mut ok = true;
        for l in replay.lines() {
            if sink.send(Message::Text(l.into())).await.is_err() {
                ok = false;
                break;
            }
        }
        while ok {
            tokio::select! {
                m = stream.next() => match m {
                    Some(Ok(Message::Text(t))) => {
                        replay.assigned(t.as_str());
                        let mut line = t.as_bytes().to_vec();
                        line.push(b'\n');
                        if to_agent.write_all(&line).await.is_err() {
                            return serving.await?;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => ok = false,
                    _ => {}
                },
                l = lines.recv() => match l {
                    Some(l) => {
                        replay.note(&l);
                        if sink.send(Message::Text(l.into())).await.is_err() {
                            ok = false;
                        }
                    }
                    None => return serving.await?,
                },
                r = &mut serving => return r?,
            }
        }
        tracing::warn!("lost the coordinator; reconnecting");
        if connected.elapsed() >= Duration::from_secs(60) {
            backoff = cfg.backoff_min;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Replay;

    #[test]
    fn the_replay_forgets_slots_and_quarantines_that_left_the_assignment() {
        let mut r = Replay::default();
        r.note(r#"{"type":"hello","host":"h","arch":"x86_64","gpus":[],"capacity":2}"#);
        r.note(r#"{"type":"slot","id":"slot-1","phase":"running","generation":1}"#);
        r.note(r#"{"type":"slot","id":"slot-2","phase":"failed","generation":1}"#);
        r.note(
            r#"{"type":"quarantine","slot":"slot-2","sbdf":"0000:19:00.0","error":"reset failed"}"#,
        );
        // The coordinator still lists both: everything is replayed.
        r.assigned(r#"{"type":"assign","slots":[{"id":"slot-1"},{"id":"slot-2"}]}"#);
        assert_eq!(r.lines().len(), 4);
        // The quarantined GPU's slot leaves the assignment.
        r.assigned(r#"{"type":"assign","slots":[{"id":"slot-1"}],"region":null}"#);
        let lines = r.lines().join("\n");
        assert!(lines.contains("slot-1"), "{lines}");
        assert!(!lines.contains("quarantine"), "{lines}");
        assert!(!lines.contains("\"slot-2\""), "{lines}");
        // An operator releases it and it runs again: nothing replays the old quarantine.
        r.assigned(r#"{"type":"assign","slots":[{"id":"slot-1"},{"id":"slot-2"}]}"#);
        r.note(r#"{"type":"slot","id":"slot-2","phase":"running","generation":2}"#);
        let lines = r.lines().join("\n");
        assert!(!lines.contains("quarantine"), "{lines}");
        // Scenes and budgets change nothing.
        r.assigned(r#"{"type":"scene","epoch_unix_ms":1}"#);
        assert_eq!(r.lines().len(), 3);
    }
}
