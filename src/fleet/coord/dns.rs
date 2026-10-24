//! The slots' DNS names in the fleet's zone, written through the Cloudflare API with the
//! zone's DNS token. For every fcvm slot and EC2 tile that reported a public address:
//! `slot-<n>.<zone>` / `ec2-<n>.<zone>` AAAA (unproxied, TTL 60) and `slot-<n>-v4.<zone>` /
//! `ec2-<n>-v4.<zone>` A, proxied through Cloudflare for IPv4-only viewers. Records are
//! written only when the wanted set changes; records of that shape nobody wants are deleted.

use anyhow::{bail, Context, Result};
use fleet_core::State;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::watch;

/// The wanted records by (name, kind).
pub type Wanted = BTreeMap<(String, String), Record>;

/// One wanted record.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Record {
    pub name: String,
    /// `A` or `AAAA`.
    pub kind: String,
    pub content: String,
    pub proxied: bool,
}

/// The records the state wants, by (name, kind).
pub fn wanted(state: &State, zone: &str) -> BTreeMap<(String, String), Record> {
    let mut out = BTreeMap::new();
    let f = &state.fleet;
    let named = f
        .slots
        .iter()
        .map(|(n, s)| (format!("slot-{n}"), s))
        .chain(f.ec2.iter().map(|(n, s)| (format!("ec2-{n}"), s)));
    for (label, s) in named {
        if let Some(v6) = &s.observed.ipv6 {
            let name = format!("{label}.{zone}");
            out.insert(
                (name.clone(), "AAAA".to_string()),
                Record {
                    name,
                    kind: "AAAA".into(),
                    content: v6.clone(),
                    proxied: false,
                },
            );
        }
        if let Some(v4) = &s.observed.ipv4 {
            let name = format!("{label}-v4.{zone}");
            out.insert(
                (name.clone(), "A".to_string()),
                Record {
                    name,
                    kind: "A".into(),
                    content: v4.clone(),
                    proxied: true,
                },
            );
        }
    }
    out
}

/// Whether a name in the zone is one this module owns (`slot-<n>`, `ec2-<n>`, with `-v4`).
fn owned(name: &str, zone: &str) -> bool {
    let Some(label) = name.strip_suffix(&format!(".{zone}")) else {
        return false;
    };
    let label = label.strip_suffix("-v4").unwrap_or(label);
    let num = label
        .strip_prefix("slot-")
        .or_else(|| label.strip_prefix("ec2-"));
    num.is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

pub struct Cloudflare {
    token: String,
    pub zone: String,
    zone_id: Option<String>,
    base: String,
    http: reqwest::Client,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct Existing {
    id: String,
    name: String,
    #[serde(rename = "type")]
    kind: String,
    content: String,
    #[serde(default)]
    proxied: bool,
}

impl Cloudflare {
    pub fn new(token: String, zone: String, zone_id: Option<String>) -> Result<Self> {
        Cloudflare::with_endpoint(token, zone, zone_id, "https://api.cloudflare.com/client/v4")
    }

    pub fn with_endpoint(
        token: String,
        zone: String,
        zone_id: Option<String>,
        base: &str,
    ) -> Result<Self> {
        Ok(Cloudflare {
            token,
            zone,
            zone_id,
            base: base.trim_end_matches('/').into(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()?,
        })
    }

    async fn api(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let mut req = self
            .http
            .request(method.clone(), format!("{}{path}", self.base))
            .bearer_auth(&self.token);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let r = req
            .send()
            .await
            .with_context(|| format!("Cloudflare {method} {path}"))?;
        let status = r.status();
        let v: serde_json::Value = r.json().await.unwrap_or_default();
        if !status.is_success() || v.get("success").and_then(|s| s.as_bool()) == Some(false) {
            bail!(
                "Cloudflare {method} {path} answered {status}: {}",
                v.get("errors").cloned().unwrap_or_default()
            );
        }
        Ok(v)
    }

    pub async fn zone_id(&mut self) -> Result<String> {
        if let Some(id) = &self.zone_id {
            return Ok(id.clone());
        }
        let v = self
            .api(
                reqwest::Method::GET,
                &format!("/zones?name={}", self.zone),
                None,
            )
            .await?;
        let id = v["result"][0]["id"]
            .as_str()
            .with_context(|| format!("no Cloudflare zone named {}", self.zone))?
            .to_string();
        self.zone_id = Some(id.clone());
        Ok(id)
    }

    async fn existing(&mut self) -> Result<Vec<Existing>> {
        let zid = self.zone_id().await?;
        let mut out = Vec::new();
        for page in 1.. {
            let v = self
                .api(
                    reqwest::Method::GET,
                    &format!("/zones/{zid}/dns_records?per_page=500&page={page}"),
                    None,
                )
                .await?;
            let batch: Vec<Existing> =
                serde_json::from_value(v["result"].clone()).unwrap_or_default();
            let n = batch.len();
            out.extend(batch.into_iter().filter(|r| owned(&r.name, &self.zone)));
            let pages = v["result_info"]["total_pages"].as_u64().unwrap_or(1);
            if n == 0 || page as u64 >= pages {
                break;
            }
        }
        Ok(out)
    }

    /// Make the zone's owned records exactly `want`; returns the changes made.
    pub async fn reconcile(&mut self, want: &BTreeMap<(String, String), Record>) -> Result<usize> {
        let zid = self.zone_id().await?;
        let have = self.existing().await?;
        let mut changes = 0;
        let mut seen = std::collections::BTreeSet::new();
        for e in &have {
            let key = (e.name.clone(), e.kind.clone());
            match want.get(&key) {
                Some(w) if !seen.contains(&key) => {
                    seen.insert(key);
                    if w.content != e.content || w.proxied != e.proxied {
                        self.api(
                            reqwest::Method::PUT,
                            &format!("/zones/{zid}/dns_records/{}", e.id),
                            Some(body(w)),
                        )
                        .await?;
                        changes += 1;
                    }
                }
                // Unwanted, or a duplicate of one already kept.
                _ => {
                    self.api(
                        reqwest::Method::DELETE,
                        &format!("/zones/{zid}/dns_records/{}", e.id),
                        None,
                    )
                    .await?;
                    changes += 1;
                }
            }
        }
        for (key, w) in want {
            if !seen.contains(key) {
                self.api(
                    reqwest::Method::POST,
                    &format!("/zones/{zid}/dns_records"),
                    Some(body(w)),
                )
                .await?;
                changes += 1;
            }
        }
        Ok(changes)
    }

    /// A TXT record (the ACME DNS-01 challenge); returns its id.
    pub async fn add_txt(&mut self, name: &str, content: &str) -> Result<String> {
        let zid = self.zone_id().await?;
        let v = self
            .api(
                reqwest::Method::POST,
                &format!("/zones/{zid}/dns_records"),
                Some(
                    serde_json::json!({"type": "TXT", "name": name, "content": content, "ttl": 60}),
                ),
            )
            .await?;
        v["result"]["id"]
            .as_str()
            .map(str::to_string)
            .context("Cloudflare named no record")
    }

    pub async fn delete(&mut self, id: &str) -> Result<()> {
        let zid = self.zone_id().await?;
        self.api(
            reqwest::Method::DELETE,
            &format!("/zones/{zid}/dns_records/{id}"),
            None,
        )
        .await
        .map(|_| ())
    }
}

fn body(r: &Record) -> serde_json::Value {
    serde_json::json!({
        "type": r.kind,
        "name": r.name,
        "content": r.content,
        "proxied": r.proxied,
        // Proxied records take Cloudflare's automatic TTL (1).
        "ttl": if r.proxied { 1 } else { 60 },
    })
}

/// Follow the wanted records and reconcile whenever they change (retrying a failed attempt).
pub async fn run(
    mut cf: Cloudflare,
    mut wanted: watch::Receiver<BTreeMap<(String, String), Record>>,
) {
    let mut done: Option<BTreeMap<(String, String), Record>> = None;
    let mut backoff = Duration::from_secs(5);
    loop {
        let want = wanted.borrow_and_update().clone();
        if done.as_ref() != Some(&want) {
            match cf.reconcile(&want).await {
                Ok(n) => {
                    if n > 0 {
                        tracing::info!(changes = n, records = want.len(), "slot DNS updated");
                    }
                    done = Some(want);
                    backoff = Duration::from_secs(5);
                }
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "slot DNS not updated; retrying");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(300));
                    continue;
                }
            }
        }
        if wanted.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::owned;

    #[test]
    fn only_slot_and_tile_names_are_owned() {
        let z = "fcvm.dev";
        assert!(owned("slot-3.fcvm.dev", z));
        assert!(owned("slot-12-v4.fcvm.dev", z));
        assert!(owned("ec2-1.fcvm.dev", z));
        assert!(!owned("stage.fcvm.dev", z));
        assert!(!owned("slot-.fcvm.dev", z));
        assert!(!owned("slot-3a.fcvm.dev", z));
        assert!(!owned("slot-3.other.dev", z));
        assert!(!owned("fleet-api-stage.fcvm.dev", z));
    }
}
