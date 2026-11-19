//! The coordinator's CloudWatch metrics: every [`EVERY`] one embedded metric format line on
//! stdout (the log stream; tracing writes to stderr), in namespace [`EMF_NAMESPACE`] with a
//! `Fleet` dimension. Every pass emits it, with or without a spend ceiling.
//!
//! | metric | value |
//! |---|---|
//! | `ActiveViewers` | joined viewers whose page is visible (or has not said), as `fleet.power` counts them |
//! | `VerifiedViewers` | joined viewers, hidden pages included. With Turnstile on, a connection joins only once verified (Turnstile, a seat ticket or an Access token); without it, every connection joins |
//! | `Power` | 0 asleep, 1 waking, 2 awake |
//! | `SpendUsdToday` | today's spend (UTC), USD; only with a spend ceiling (`--daily-spend-usd`) |
//! | `AwakeHosts` | hosts whose agent is connected, reported at least one GPU and is not draining |
//! | `BroadcastLive` | 1 when IVS answers that the broadcast channel is live, else 0 (offline, or IVS could not be asked); only with a broadcast whose secret names its `channel_arn` |
//! | `CertDaysLeft` | days until the tile certificate in `--tls-secret` expires; only with a certificate secret, from its first read |
//!
//! The first five come from the coordinator's snapshot. The broadcast is asked of IVS every
//! pass (`GetStream`). The certificate secret is read hourly, and again 5 minutes after a
//! failed read; until a read succeeds again the last expiry read stays in use.

use super::aws::{signed_with_headers, CredentialChain};
use super::secrets::Secrets;
use super::Snapshot;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

pub const EMF_NAMESPACE: &str = "FCVM/Fleet";

/// Time between two lines.
pub const EVERY: Duration = Duration::from_secs(30);
/// Time between two reads of the certificate secret, and after a failed one.
const CERT_READ_EVERY: Duration = Duration::from_secs(3600);
const CERT_RETRY: Duration = Duration::from_secs(300);

/// The tile certificate's expiry.
#[async_trait]
pub trait CertSource: Send + Sync {
    async fn not_after(&self) -> Result<jiff::Timestamp>;
}

/// Whether the broadcast is being published.
#[async_trait]
pub trait BroadcastSource: Send + Sync {
    async fn live(&self) -> Result<bool>;
}

pub struct MetricsConfig {
    /// The `Fleet` dimension.
    pub fleet: String,
    pub every: Duration,
    pub cert: Option<Arc<dyn CertSource>>,
    pub broadcast: Option<Arc<dyn BroadcastSource>>,
}

/// What the snapshot does not hold, read by the metrics task itself.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Probed {
    /// `None` without a broadcast to ask about.
    pub broadcast_live: Option<bool>,
    /// `None` without a certificate secret, or before its first read.
    pub cert_not_after: Option<jiff::Timestamp>,
}

/// The parts of the snapshot (`fleet_core::view::View`) the metrics read.
#[derive(Deserialize)]
struct SnapshotMsg {
    state: ViewPart,
}

#[derive(Deserialize)]
struct ViewPart {
    viewers: ViewersPart,
    fleet: FleetPart,
}

#[derive(Deserialize)]
struct ViewersPart {
    count: u64,
    active: u64,
}

#[derive(Deserialize)]
struct FleetPart {
    hosts: Vec<HostPart>,
    power: PowerPart,
    #[serde(default)]
    spend: Option<SpendPart>,
}

#[derive(Deserialize)]
struct HostPart {
    gpus: Vec<serde_json::Value>,
    draining: bool,
}

#[derive(Deserialize)]
struct PowerPart {
    state: String,
}

#[derive(Deserialize)]
struct SpendPart {
    usd_today: f64,
}

/// The metric line for one pass, from a snapshot's text; `None` when it is not a snapshot
/// (before the first one).
pub fn emf_line(snapshot: &str, fleet: &str, now_ms: u64, probed: &Probed) -> Option<String> {
    let s: SnapshotMsg = serde_json::from_str(snapshot).ok()?;
    let f = &s.state.fleet;
    let power = match f.power.state.as_str() {
        "asleep" => 0,
        "waking" => 1,
        _ => 2,
    };
    let mut metrics: Vec<(&str, &str, serde_json::Value)> = vec![
        ("ActiveViewers", "Count", s.state.viewers.active.into()),
        ("VerifiedViewers", "Count", s.state.viewers.count.into()),
        ("Power", "None", power.into()),
    ];
    if let Some(spend) = &f.spend {
        metrics.push(("SpendUsdToday", "None", spend.usd_today.into()));
    }
    let awake = f
        .hosts
        .iter()
        .filter(|h| !h.draining && !h.gpus.is_empty())
        .count() as u64;
    metrics.push(("AwakeHosts", "Count", awake.into()));
    if let Some(live) = probed.broadcast_live {
        metrics.push(("BroadcastLive", "None", u8::from(live).into()));
    }
    if let Some(t) = probed.cert_not_after {
        let secs = t.as_second() - (now_ms / 1000) as i64;
        let days = (secs as f64 / 86_400.0 * 100.0).round() / 100.0;
        metrics.push(("CertDaysLeft", "None", days.into()));
    }
    let mut line = serde_json::json!({
        "_aws": {
            "Timestamp": now_ms,
            "CloudWatchMetrics": [{
                "Namespace": EMF_NAMESPACE,
                "Dimensions": [["Fleet"]],
                "Metrics": metrics
                    .iter()
                    .map(|(name, unit, _)| serde_json::json!({"Name": name, "Unit": unit}))
                    .collect::<Vec<_>>(),
            }],
        },
        "Fleet": fleet,
    });
    for (name, _, value) in metrics {
        line[name] = value;
    }
    Some(line.to_string())
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Write one line to `out` every `cfg.every` until the snapshot's sender is gone.
pub async fn run(
    cfg: MetricsConfig,
    snapshot: watch::Receiver<Arc<Snapshot>>,
    mut out: impl FnMut(String),
) {
    let mut probed = Probed::default();
    let mut cert_due = Instant::now();
    let mut broadcast_failing = false;
    let mut every = tokio::time::interval(cfg.every);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        if snapshot.has_changed().is_err() {
            return;
        }
        if let Some(c) = &cfg.cert {
            if Instant::now() >= cert_due {
                match c.not_after().await {
                    Ok(t) => {
                        probed.cert_not_after = Some(t);
                        cert_due = Instant::now() + CERT_READ_EVERY;
                    }
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), "the certificate's expiry could not be read");
                        cert_due = Instant::now() + CERT_RETRY;
                    }
                }
            }
        }
        if let Some(b) = &cfg.broadcast {
            let live = match b.live().await {
                Ok(live) => {
                    broadcast_failing = false;
                    live
                }
                Err(e) => {
                    if !broadcast_failing {
                        tracing::warn!(error = %format!("{e:#}"), "IVS could not be asked whether the broadcast is live");
                    }
                    broadcast_failing = true;
                    false
                }
            };
            probed.broadcast_live = Some(live);
        }
        let text = snapshot.borrow().text.clone();
        if let Some(line) = emf_line(&text, &cfg.fleet, unix_ms(), &probed) {
            out(line);
        }
    }
}

/// The certificate's expiry from its secret (`not_after`, the format fleet/tools/issue-cert.sh
/// and `cert::doc_from_pem` write).
pub struct SecretCert {
    pub secrets: Secrets,
    pub id: String,
}

/// The `not_after` of a certificate secret's text.
pub fn secret_not_after(text: &str) -> Result<jiff::Timestamp> {
    #[derive(Deserialize)]
    struct Doc {
        not_after: String,
    }
    let doc: Doc = serde_json::from_str(text).context("the certificate secret has no not_after")?;
    doc.not_after
        .parse()
        .with_context(|| format!("the certificate's not_after {:?}", doc.not_after))
}

#[async_trait]
impl CertSource for SecretCert {
    async fn not_after(&self) -> Result<jiff::Timestamp> {
        secret_not_after(&self.secrets.get(&self.id).await?)
    }
}

/// The broadcast's state as IVS has it (`GetStream` on the channel).
pub struct IvsStream {
    channel_arn: String,
    region: String,
    base: String,
    chain: CredentialChain,
    http: reqwest::Client,
}

/// The region of an IVS channel's ARN (`arn:aws:ivs:<region>:<account>:channel/<id>`).
pub fn ivs_region(arn: &str) -> Result<String> {
    let parts: Vec<&str> = arn.split(':').collect();
    match parts.as_slice() {
        ["arn", _, "ivs", region, _, resource]
            if !region.is_empty() && resource.starts_with("channel/") =>
        {
            Ok(region.to_string())
        }
        _ => anyhow::bail!("{arn:?} is not an IVS channel ARN"),
    }
}

/// Whether a `GetStream` answer says the channel is live: 200 with `stream.state` `LIVE`.
/// `ChannelNotBroadcasting` (404) is offline; any other answer is an error.
pub fn get_stream_live(status: u16, error_type: Option<&str>, body: &[u8]) -> Result<bool> {
    if status == 200 {
        let v: serde_json::Value =
            serde_json::from_slice(body).context("GetStream's answer is not JSON")?;
        return Ok(v["stream"]["state"].as_str() == Some("LIVE"));
    }
    if status == 404 && error_type.is_some_and(|t| t.starts_with("ChannelNotBroadcasting")) {
        return Ok(false);
    }
    anyhow::bail!(
        "IVS GetStream answered {status} {}",
        error_type.unwrap_or("")
    )
}

impl IvsStream {
    pub fn new(channel_arn: &str) -> Result<Self> {
        let region = ivs_region(channel_arn)?;
        Ok(IvsStream {
            channel_arn: channel_arn.to_string(),
            base: format!("https://ivs.{region}.amazonaws.com"),
            region,
            chain: CredentialChain::new()?,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()?,
        })
    }
}

#[async_trait]
impl BroadcastSource for IvsStream {
    async fn live(&self) -> Result<bool> {
        let (status, headers, body) = signed_with_headers(
            &self.chain,
            &self.http,
            &self.base,
            reqwest::Method::POST,
            "/GetStream",
            "",
            &self.region,
            "ivs",
            &[("content-type", "application/json".into())],
            serde_json::to_vec(&serde_json::json!({"channelArn": self.channel_arn}))?,
        )
        .await?;
        let error_type = headers
            .get("x-amzn-errortype")
            .and_then(|v| v.to_str().ok());
        get_stream_live(status.as_u16(), error_type, &body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_core::state::{Config, State};

    /// A fleet with three hosts (one draining, one with no GPU), three viewers (one hidden) and, with
    /// `ceiling`, a spend ledger.
    fn fixture(ceiling: Option<f64>) -> State {
        let mut state = State::new(
            Config {
                daily_spend_usd: ceiling,
                ..Config::default()
            },
            0,
        );
        let t4 = serde_json::json!([{"name": "Tesla T4", "sbdf": "0000:01:00.0"}]);
        // h3 reports no GPU: it serves no tile.
        for (host, gpus) in [("h1", &t4), ("h2", &t4), ("h3", &serde_json::json!([]))] {
            let hello: fleet_core::Event = serde_json::from_value(serde_json::json!({
                "at": 0, "type": "agent", "host": host,
                "msg": {"type": "hello", "host": host, "arch": "x86_64", "capacity": 1,
                        "gpus": gpus}}))
            .unwrap();
            state = fleet_core::apply(state, &hello);
        }
        let f = &mut state.fleet;
        f.hosts.get_mut("h2").unwrap().draining = true;
        for v in ["v-1", "v-2", "v-3"] {
            f.viewers.insert(v.into(), "198.51.100.1".into());
        }
        f.hidden.insert("v-3".into());
        f.power.state = "waking".into();
        f.spend.usd = 12.34;
        state
    }

    #[test]
    fn the_metric_line_carries_every_metric_with_the_fleet_dimension() {
        let snap = crate::fleet::coord::snapshot_of(&fixture(Some(750.0)));
        let probed = Probed {
            broadcast_live: Some(true),
            cert_not_after: Some("2023-12-04T22:13:20Z".parse().unwrap()),
        };
        // 2023-11-14T22:13:20Z: 20 days before the certificate expires.
        let line = emf_line(&snap.text, "prod", 1_700_000_000_000, &probed).unwrap();
        assert_eq!(
            line,
            concat!(
                r#"{"ActiveViewers":2,"AwakeHosts":1,"BroadcastLive":1,"CertDaysLeft":20.0,"#,
                r#""Fleet":"prod","Power":1,"SpendUsdToday":12.34,"VerifiedViewers":3,"#,
                r#""_aws":{"CloudWatchMetrics":[{"Dimensions":[["Fleet"]],"Metrics":["#,
                r#"{"Name":"ActiveViewers","Unit":"Count"},"#,
                r#"{"Name":"VerifiedViewers","Unit":"Count"},"#,
                r#"{"Name":"Power","Unit":"None"},"#,
                r#"{"Name":"SpendUsdToday","Unit":"None"},"#,
                r#"{"Name":"AwakeHosts","Unit":"Count"},"#,
                r#"{"Name":"BroadcastLive","Unit":"None"},"#,
                r#"{"Name":"CertDaysLeft","Unit":"None"}],"#,
                r#""Namespace":"FCVM/Fleet"}],"Timestamp":1700000000000}}"#,
            )
        );
        // Not a snapshot yet: no line.
        assert_eq!(emf_line("", "prod", 0, &probed), None);
    }

    struct Fixed(Result<bool, String>);

    #[async_trait]
    impl BroadcastSource for Fixed {
        async fn live(&self) -> Result<bool> {
            self.0.clone().map_err(anyhow::Error::msg)
        }
    }

    /// A pass of a fleet with no spend ceiling, no certificate secret and a broadcast IVS
    /// cannot be asked about.
    #[tokio::test]
    async fn a_pass_without_a_ceiling_emits_the_viewer_and_host_metrics() {
        let (tx, rx) = watch::channel(crate::fleet::coord::snapshot_of(&fixture(None)));
        let cfg = MetricsConfig {
            fleet: "prod".into(),
            every: Duration::from_millis(5),
            cert: None,
            broadcast: Some(Arc::new(Fixed(Err("denied".into())))),
        };
        let (lines_tx, mut lines) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(run(cfg, rx, move |l| {
            let _ = lines_tx.send(l);
        }));
        let line = tokio::time::timeout(Duration::from_secs(10), lines.recv())
            .await
            .expect("a line within 10 s")
            .expect("a line");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["ActiveViewers"], 2);
        assert_eq!(v["VerifiedViewers"], 3);
        assert_eq!(v["AwakeHosts"], 1);
        assert_eq!(v["Power"], 1);
        assert_eq!(v["BroadcastLive"], 0, "IVS not asked: not live");
        assert!(v.get("SpendUsdToday").is_none(), "no ceiling, no spend");
        assert!(v.get("CertDaysLeft").is_none(), "no certificate secret");
        let names: Vec<&str> = v["_aws"]["CloudWatchMetrics"][0]["Metrics"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["Name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "ActiveViewers",
                "VerifiedViewers",
                "Power",
                "AwakeHosts",
                "BroadcastLive"
            ]
        );
        // The coordinator gone: the task ends.
        drop(tx);
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("the task ends with the snapshot's sender")
            .unwrap();
    }

    #[test]
    fn get_stream_says_live_only_for_a_live_stream() {
        let live = br#"{"stream":{"channelArn":"x","health":"HEALTHY","state":"LIVE"}}"#;
        assert!(get_stream_live(200, None, live).unwrap());
        let off = br#"{"stream":{"state":"OFFLINE"}}"#;
        assert!(!get_stream_live(200, None, off).unwrap());
        assert!(!get_stream_live(404, Some("ChannelNotBroadcasting"), b"{}").unwrap());
        assert!(get_stream_live(404, Some("ResourceNotFoundException"), b"{}").is_err());
        assert!(get_stream_live(403, Some("AccessDeniedException"), b"{}").is_err());
    }

    #[test]
    fn the_region_comes_from_the_channel_arn() {
        assert_eq!(
            ivs_region("arn:aws:ivs:us-east-1:123456789012:channel/abcdEFGH").unwrap(),
            "us-east-1"
        );
        for bad in [
            "arn:aws:ivs::123456789012:channel/x",
            "arn:aws:s3:us-east-1:123456789012:channel/x",
            "arn:aws:ivs:us-east-1:123456789012:stream-key/x",
            "channel/x",
        ] {
            assert!(ivs_region(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_certificate_expiry_is_the_secrets_not_after() {
        let doc = serde_json::json!({"cert": "", "key": "", "domains": [],
            "not_after": "2026-12-01T00:00:00Z", "issued_at": "2026-09-02T00:00:00Z",
            "issuer": "CN=Test"})
        .to_string();
        assert_eq!(
            secret_not_after(&doc).unwrap(),
            "2026-12-01T00:00:00Z".parse::<jiff::Timestamp>().unwrap()
        );
        assert!(secret_not_after(r#"{"cert": ""}"#).is_err());
        assert!(secret_not_after(r#"{"not_after": "soon"}"#).is_err());
    }
}
