//! `fcvm fleet agent --dial <wss url>`: the agent dials the coordinator and speaks the same
//! messages as over stdio, one JSON message per WebSocket text frame.
//!
//! The connection can drop (a coordinator restart, a network blip) while the host's VMs keep
//! running: the agent itself never stops. Each new connection starts with the hello and
//! then the latest of every message that describes current state (each slot's report, the
//! aggregator, a draining notice, quarantines), so a coordinator that lost the host learns
//! where every slot is without restarting a VM. A quarantine is replayed until the
//! coordinator says a stored state holds it (`quarantine_stored`): a coordinator that
//! crashed before writing it would otherwise assign the GPU again. Reconnects back off from
//! 1 s to 30 s.

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

    /// A coordinator frame. An `assign` names every slot the host still runs: a slot that
    /// left it is torn down here, so its last report is no longer true, and replaying it
    /// would report a removed slot running. A quarantine stays until `quarantine_stored`
    /// names its GPU: the coordinator sends that once a stored state holds the quarantine,
    /// or an operator's release that followed it, so neither a coordinator restart can lose
    /// it nor a later replay quarantine a released GPU again.
    fn coordinator_frame(&mut self, frame: &str) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(frame) else {
            return;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("assign") => {
                let ids: std::collections::BTreeSet<&str> = v["slots"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|s| s.get("id").and_then(|i| i.as_str()))
                    .collect();
                self.keyed.retain(|key, _| match key.strip_prefix("slot/") {
                    Some(id) => ids.contains(id),
                    None => true,
                });
            }
            // Only the report it names: a newer failure of the same GPU (after an operator
            // released it) replaced the line and is not stored yet.
            Some("quarantine_stored") => {
                let Some(sbdf) = v.get("sbdf").and_then(|s| s.as_str()) else {
                    return;
                };
                let key = format!("quarantine/{sbdf}");
                let report = |v: &serde_json::Value| v.get("report").cloned();
                let named = self.keyed.get(&key).is_some_and(|line| {
                    serde_json::from_str::<serde_json::Value>(line)
                        .is_ok_and(|q| report(&q) == report(&v))
                });
                if named {
                    self.keyed.remove(&key);
                }
            }
            _ => {}
        }
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
                        replay.coordinator_frame(t.as_str());
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

    /// An operator releases a GPU while its first quarantine waits for its acknowledgement,
    /// and the GPU fails again: the second report replaces the first in the replay, and the
    /// first one's acknowledgement must not drop it.
    #[test]
    fn an_acknowledgement_drops_only_the_report_it_names() {
        let mut r = Replay::default();
        r.note(r#"{"type":"hello","host":"h","arch":"x86_64","gpus":[],"capacity":1}"#);
        r.note(r#"{"type":"quarantine","slot":"slot-1","sbdf":"0000:19:00.0","error":"a","report":"r1"}"#);
        r.note(r#"{"type":"quarantine","slot":"slot-1","sbdf":"0000:19:00.0","error":"b","report":"r2"}"#);
        r.coordinator_frame(r#"{"type":"quarantine_stored","sbdf":"0000:19:00.0","report":"r1"}"#);
        let lines = r.lines().join("\n");
        assert!(lines.contains("\"report\":\"r2\""), "{lines}");
        r.coordinator_frame(r#"{"type":"quarantine_stored","sbdf":"0000:19:00.0","report":"r2"}"#);
        assert!(!r.lines().join("\n").contains("quarantine"));
    }

    #[test]
    fn the_replay_forgets_slots_that_left_and_keeps_a_quarantine_until_it_is_stored() {
        let mut r = Replay::default();
        r.note(r#"{"type":"hello","host":"h","arch":"x86_64","gpus":[],"capacity":2}"#);
        r.note(r#"{"type":"slot","id":"slot-1","phase":"running","generation":1}"#);
        r.note(r#"{"type":"slot","id":"slot-2","phase":"failed","generation":1}"#);
        r.note(
            r#"{"type":"quarantine","slot":"slot-2","sbdf":"0000:19:00.0","error":"reset failed","report":"r1"}"#,
        );
        // The coordinator still lists both: everything is replayed.
        r.coordinator_frame(r#"{"type":"assign","slots":[{"id":"slot-1"},{"id":"slot-2"}]}"#);
        assert_eq!(r.lines().len(), 4);
        // The quarantined GPU's slot leaves the assignment: its report goes, its quarantine
        // stays, since the coordinator may not have stored it yet.
        r.coordinator_frame(r#"{"type":"assign","slots":[{"id":"slot-1"}],"region":null}"#);
        let lines = r.lines().join("\n");
        assert!(lines.contains("slot-1"), "{lines}");
        assert!(!lines.contains("\"id\":\"slot-2\""), "{lines}");
        assert!(lines.contains("\"quarantine\""), "{lines}");
        // Another GPU's acknowledgement, or another report's, changes nothing.
        r.coordinator_frame(r#"{"type":"quarantine_stored","sbdf":"0000:01:00.0","report":"r1"}"#);
        r.coordinator_frame(r#"{"type":"quarantine_stored","sbdf":"0000:19:00.0","report":"r0"}"#);
        assert!(r.lines().join("\n").contains("\"quarantine\""));
        // Stored: no longer replayed.
        r.coordinator_frame(r#"{"type":"quarantine_stored","sbdf":"0000:19:00.0","report":"r1"}"#);
        assert!(!r.lines().join("\n").contains("quarantine"));
        // An operator releases it and it runs again: nothing replays the old quarantine.
        r.coordinator_frame(r#"{"type":"assign","slots":[{"id":"slot-1"},{"id":"slot-2"}]}"#);
        r.note(r#"{"type":"slot","id":"slot-2","phase":"running","generation":2}"#);
        let lines = r.lines().join("\n");
        assert!(!lines.contains("quarantine"), "{lines}");
        // Scenes and budgets change nothing.
        r.coordinator_frame(r#"{"type":"scene","epoch_unix_ms":1}"#);
        assert_eq!(r.lines().len(), 3);
    }
}
