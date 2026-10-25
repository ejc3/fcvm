//! The fleet's launch names, and the Cloudflare API client the certificate renewal writes its
//! DNS-01 records with. Every launch of an fcvm slot or EC2 tile gets a label never used
//! before, `slot-<n>-<token>` / `ec2-<n>-<token>` (`token`: 8 random base32 characters), and
//! two names under it in the launch names' subzone, which the coordinator serves itself
//! ([`super::authority`]):
//!
//! - `<label>.<subzone>` AAAA: the slot's own IPv6 address (a routed VM's /128, a tile's).
//! - `<label>-v4.<subzone>` A: an IPv4 address: a GPU host's public address, shared by its
//!   slots, each forwarded on its own port (`SlotReport::v4_port`), or a tile's own.
//!
//! A name is never answered before it is minted, so no resolver holds a negative answer for
//! it, and never changes address, so none holds a stale positive one.

use anyhow::{bail, Context, Result};
use fleet_core::state::Published;
use fleet_core::State;
use std::time::Duration;

/// What the DNS task needs about one slot or tile.
#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    /// `slot-<n>` or `ec2-<n>`.
    pub id: String,
    /// The launch it is on (its observed generation).
    pub generation: u64,
    pub ipv6: Option<String>,
    pub ipv4: Option<String>,
    pub v4_port: Option<u16>,
    pub published: Option<Published>,
}

/// The facts of every slot and tile in the fleet.
pub fn facts(state: &State) -> Vec<Fact> {
    let f = &state.fleet;
    f.slots
        .values()
        .chain(f.ec2.values())
        .map(|s| Fact {
            id: s.id.clone(),
            generation: s.observed.generation,
            ipv6: s.observed.ipv6.clone(),
            ipv4: s.observed.ipv4.clone(),
            v4_port: s.observed.v4_port,
            published: s.published.clone(),
        })
        .collect()
}

/// An address in its canonical text (`2001:db8::2` for `2001:0db8::2`), so a report and the
/// published names compare as the same address; text that is not an address is kept as it is.
fn canonical(a: &str) -> String {
    a.parse::<std::net::IpAddr>()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|_| a.to_string())
}

/// Whether `p` names exactly the launch `f` is on: its generation, its addresses and the
/// IPv4 port in its URL. Anything else is a new launch, with a new label.
pub fn is_current(zone: &str, p: &Published, f: &Fact) -> bool {
    *p == names(
        zone,
        &p.label,
        f.generation,
        f.ipv6.as_deref(),
        f.ipv4.as_deref(),
        f.v4_port,
    )
}

/// A launch's names: the label, its addresses and the frame URLs they give.
pub fn names(
    zone: &str,
    label: &str,
    generation: u64,
    ipv6: Option<&str>,
    ipv4: Option<&str>,
    v4_port: Option<u16>,
) -> Published {
    let port = match v4_port {
        Some(p) if p != 443 => format!(":{p}"),
        _ => String::new(),
    };
    Published {
        generation,
        label: label.into(),
        ipv6: ipv6.map(canonical),
        ipv4: ipv4.map(canonical),
        v4_port,
        frame_url: ipv6.map(|_| format!("wss://{label}.{zone}/frames")),
        frame_url_v4: ipv4.map(|_| format!("wss://{label}-v4.{zone}{port}/frames")),
    }
}

/// An address a slot reported that is not one of its family (`ipv6` must be IPv6, `ipv4`
/// IPv4): naming it would only publish a record nobody can use.
pub fn bad_address(f: &Fact) -> Option<&str> {
    [(&f.ipv6, true), (&f.ipv4, false)]
        .into_iter()
        .find(|(a, v6)| {
            a.as_ref()
                .is_some_and(|a| match a.parse::<std::net::IpAddr>() {
                    Ok(ip) => ip.is_ipv6() != *v6,
                    Err(_) => true,
                })
        })
        .and_then(|(a, _)| a.as_deref())
}

/// Whether a slot needs names for the launch it is on: it has an address, every address it
/// reported is one, and it has no published names for exactly this launch.
pub fn needs_names(zone: &str, f: &Fact) -> bool {
    (f.ipv6.is_some() || f.ipv4.is_some())
        && bad_address(f).is_none()
        && !f.published.as_ref().is_some_and(|p| is_current(zone, p, f))
}

/// A label for one launch of `id`, never made before (8 random base32 characters).
pub fn mint(id: &str) -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut r = rand::thread_rng();
    let token: String = (0..8)
        .map(|_| ALPHABET[r.gen_range(0..ALPHABET.len())] as char)
        .collect();
    format!("{id}-{token}")
}

pub struct Cloudflare {
    token: String,
    pub zone: String,
    zone_id: Option<String>,
    base: String,
    http: reqwest::Client,
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

#[cfg(test)]
mod tests {
    use super::{mint, names};

    #[test]
    fn labels_have_the_fleet_shape_and_are_not_reused() {
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..1000 {
            let l = mint("slot-3");
            let token = l.strip_prefix("slot-3-").expect("the slot's id");
            assert_eq!(token.len(), 8, "{l}");
            assert!(
                token
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b)),
                "{l}"
            );
            assert!(seen.insert(l.clone()), "minted twice: {l}");
        }
    }

    #[test]
    fn a_launch_names_its_frame_urls_with_the_v4_port() {
        let p = names(
            "fcvm.dev",
            "slot-1-abcdefgh",
            3,
            Some("2001:db8::2"),
            Some("192.0.2.1"),
            Some(20200),
        );
        assert_eq!(
            p.frame_url.as_deref(),
            Some("wss://slot-1-abcdefgh.fcvm.dev/frames")
        );
        assert_eq!(
            p.frame_url_v4.as_deref(),
            Some("wss://slot-1-abcdefgh-v4.fcvm.dev:20200/frames")
        );
        let t = names(
            "fcvm.dev",
            "ec2-2-abcdefgh",
            1,
            None,
            Some("192.0.2.9"),
            Some(443),
        );
        assert_eq!(t.frame_url, None);
        assert_eq!(
            t.frame_url_v4.as_deref(),
            Some("wss://ec2-2-abcdefgh-v4.fcvm.dev/frames")
        );
    }

    #[test]
    fn addresses_are_canonical_and_a_new_port_is_a_new_launch() {
        use super::{is_current, Fact};
        let p = names(
            "fcvm.dev",
            "ec2-1-abcdefgh",
            2,
            Some("2001:0db8:0::2"),
            Some("192.0.2.1"),
            None,
        );
        assert_eq!(p.ipv6.as_deref(), Some("2001:db8::2"));
        let mut f = Fact {
            id: "ec2-1".into(),
            generation: 2,
            ipv6: Some("2001:db8::0:2".into()),
            ipv4: Some("192.0.2.1".into()),
            v4_port: None,
            published: None,
        };
        assert!(is_current("fcvm.dev", &p, &f));
        f.v4_port = Some(20201);
        assert!(
            !is_current("fcvm.dev", &p, &f),
            "a new IPv4 port must be a new launch"
        );
        f.v4_port = None;
        f.generation = 3;
        assert!(!is_current("fcvm.dev", &p, &f));
    }
}
