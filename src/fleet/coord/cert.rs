//! Renewal of the fleet's wildcard certificate, kept in a Secrets Manager secret in the
//! format fleet/tools/issue-cert.sh writes: a JSON object with `cert` (PEM full chain), `key`
//! (PEM private key), `domains`, `not_after`, `issued_at` (RFC 3339 UTC) and `issuer`. Twice a
//! day the coordinator reads it; within 30 days of expiry it orders a new certificate from the
//! ACME directory (Let's Encrypt), answers DNS-01 with TXT records, stores the result as the
//! secret's new version and removes the TXT records. The key and certificate stay in memory.
//!
//! What it covers is `--tls-scope` ([`TlsScope`]): the zone (`*.<zone>`, `<zone>` and the
//! launch subzone's wildcard), with the TXT records written in the zone through its Cloudflare
//! token, or the launch subzone's wildcard alone, with the TXT record answered by the
//! coordinator's own DNS server for the subzone ([`ChallengeTxt`]), so it holds no token that
//! can write the parent zone and no key it holds is valid for a name in front of Cloudflare.

use super::dns::Cloudflare;
use super::secrets::Secrets;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use instant_acme::{
    Account, AuthorizationStatus, ChallengeType, Identifier, NewAccount, NewOrder, OrderStatus,
    RetryPolicy,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CertDoc {
    pub cert: String,
    pub key: String,
    pub domains: Vec<String>,
    pub not_after: String,
    pub issued_at: String,
    pub issuer: String,
    /// The renderers' scene token (`scene_token`), kept through every renewal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene_token: Option<String>,
}

/// Renew this long before expiry.
pub const RENEW_BEFORE: jiff::SignedDuration = jiff::SignedDuration::from_hours(24 * 30);

/// What the certificate covers (`--tls-scope`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum TlsScope {
    /// `*.<zone>`, `<zone>` and the launch subzone's wildcard; DNS-01 records in the zone.
    #[default]
    Zone,
    /// The launch subzone's wildcard alone; DNS-01 answered by the subzone's own server.
    Subzone,
}

/// Whether the stored secret needs a new certificate now: missing, unreadable, not
/// covering every name of `domains`, or expiring within `RENEW_BEFORE`.
pub fn needs_renewal(secret: Option<&str>, domains: &[String], now: jiff::Timestamp) -> bool {
    let Some(doc) = secret.and_then(|s| serde_json::from_str::<CertDoc>(s).ok()) else {
        return true;
    };
    if !domains.iter().all(|d| doc.domains.iter().any(|x| x == d)) {
        return true;
    }
    match doc.not_after.parse::<jiff::Timestamp>() {
        Ok(t) => t.duration_since(now) <= RENEW_BEFORE,
        Err(_) => true,
    }
}

/// The secret's document for a PEM chain and key: expiry, names and issuer read from the
/// leaf certificate.
pub fn doc_from_pem(chain: &str, key: &str, now: jiff::Timestamp) -> Result<CertDoc> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(chain.as_bytes())
        .map_err(|e| anyhow::anyhow!("the chain is not PEM: {e}"))?;
    let leaf = pem
        .parse_x509()
        .map_err(|e| anyhow::anyhow!("the leaf is not X.509: {e}"))?;
    let not_after = jiff::Timestamp::from_second(leaf.validity().not_after.timestamp())
        .context("the leaf's expiry")?;
    let mut domains = Vec::new();
    if let Ok(Some(san)) = leaf.subject_alternative_name() {
        for n in &san.value.general_names {
            if let x509_parser::extensions::GeneralName::DNSName(d) = n {
                domains.push(d.to_string());
            }
        }
    }
    let fmt = |t: jiff::Timestamp| t.strftime("%Y-%m-%dT%H:%M:%SZ").to_string();
    Ok(CertDoc {
        cert: chain.to_string(),
        key: key.to_string(),
        domains,
        not_after: fmt(not_after),
        issued_at: fmt(now),
        issuer: leaf.issuer().to_string(),
        scene_token: None,
    })
}

pub struct Renewal {
    pub secret: String,
    /// The names the certificate covers, as the order asks for them.
    pub domains: Vec<String>,
    /// The name every DNS-01 record is written at: `_acme-challenge.<zone>` for a zone
    /// certificate (the subzone's server answers the subzone's challenge with a CNAME to it),
    /// `_acme-challenge.<subzone>` for a subzone certificate.
    pub challenge: String,
    /// The certificate must name `domains` and nothing more (a subzone certificate).
    pub exact: bool,
    pub directory: String,
    pub check_every: Duration,
    /// How soon a renewal that failed is tried again (a certificate that does not cover a
    /// new name is due at once, and waiting `check_every` would leave it uncovered for half
    /// a day). Let's Encrypt allows 5 failed validations per name per hour.
    pub retry_after: Duration,
    /// Where DNS-01 answers are checked before telling the CA (a DNS-over-HTTPS JSON API).
    pub doh: String,
    /// The scene token every new version carries (`scene_token::ensure`): a renewal that
    /// dropped it would leave every renderer refusing the coordinator's scenes.
    pub scene_token: Option<String>,
}

impl Renewal {
    /// A certificate for `*.<zone>` and `<zone>`, and the subzone's wildcard when `subzone`
    /// names one, with its DNS-01 records at `_acme-challenge.<zone>`.
    pub fn zone(secret: String, zone: &str, subzone: Option<&str>) -> Self {
        let zone = zone.trim_end_matches('.').to_ascii_lowercase();
        let mut domains = vec![format!("*.{zone}"), zone.clone()];
        if let Some(sub) = subzone {
            domains.push(format!(
                "*.{}",
                sub.trim_end_matches('.').to_ascii_lowercase()
            ));
        }
        Renewal::new(secret, domains, format!("_acme-challenge.{zone}"))
    }

    /// A certificate for `*.<subzone>` alone, with its DNS-01 record at
    /// `_acme-challenge.<subzone>`, which the subzone's own server answers.
    pub fn subzone(secret: String, subzone: &str) -> Self {
        let sub = subzone.trim_end_matches('.').to_ascii_lowercase();
        Renewal {
            exact: true,
            ..Renewal::new(
                secret,
                vec![format!("*.{sub}")],
                format!("_acme-challenge.{sub}"),
            )
        }
    }

    /// Whether the stored secret needs a new certificate now ([`needs_renewal`]). With
    /// `exact`, a certificate that also names anything else does too: switching to a subzone
    /// certificate must replace a key that is valid for the parent zone.
    pub fn needs_renewal(&self, secret: Option<&str>, now: jiff::Timestamp) -> bool {
        if needs_renewal(secret, &self.domains, now) {
            return true;
        }
        self.exact
            && secret
                .and_then(|s| serde_json::from_str::<CertDoc>(s).ok())
                .is_some_and(|doc| doc.domains.iter().any(|d| !self.domains.contains(d)))
    }

    fn new(secret: String, domains: Vec<String>, challenge: String) -> Self {
        Renewal {
            secret,
            domains,
            challenge,
            exact: false,
            directory: instant_acme::LetsEncrypt::Production.url().to_string(),
            check_every: Duration::from_secs(12 * 3600),
            retry_after: Duration::from_secs(15 * 60),
            doh: "https://cloudflare-dns.com/dns-query".into(),
            scene_token: None,
        }
    }
}

/// Wait until the public DNS answers every value at `name`.
async fn propagated(doh: &str, name: &str, values: &[String]) -> Result<()> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    loop {
        let v: serde_json::Value = http
            .get(doh)
            .query(&[("name", name), ("type", "TXT")])
            .header("accept", "application/dns-json")
            .send()
            .await?
            .json()
            .await
            .unwrap_or_default();
        let seen: Vec<String> = v["Answer"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x["data"].as_str())
                    .map(|d| d.trim_matches('"').to_string())
                    .collect()
            })
            .unwrap_or_default();
        if values.iter().all(|x| seen.contains(x)) {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            bail!("{name} did not show the challenge within 300 s");
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Where DNS-01 TXT records are written.
#[async_trait]
pub trait Dns01: Send {
    /// A TXT record at `name`; returns its id.
    async fn add_txt(&mut self, name: &str, value: &str) -> Result<String>;
    async fn delete(&mut self, id: &str) -> Result<()>;
}

#[async_trait]
impl Dns01 for Cloudflare {
    async fn add_txt(&mut self, name: &str, value: &str) -> Result<String> {
        Cloudflare::add_txt(self, name, value).await
    }
    async fn delete(&mut self, id: &str) -> Result<()> {
        Cloudflare::delete(self, id).await
    }
}

/// The TXT values the subzone's DNS server answers at `_acme-challenge.<subzone>`, written by
/// the renewal of a subzone certificate (`--tls-scope subzone`). A clone shares the values.
#[derive(Clone)]
pub struct ChallengeTxt {
    /// The one name it holds records for.
    pub name: String,
    values: tokio::sync::watch::Sender<Vec<String>>,
}

impl std::fmt::Debug for ChallengeTxt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChallengeTxt")
            .field("name", &self.name)
            .field("values", &self.values())
            .finish()
    }
}

/// Two handles are equal when they name the same challenge and hold the same values.
impl PartialEq for ChallengeTxt {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.values() == other.values()
    }
}

impl ChallengeTxt {
    pub fn new(name: &str) -> Self {
        ChallengeTxt {
            name: name.trim_end_matches('.').to_ascii_lowercase(),
            values: tokio::sync::watch::channel(Vec::new()).0,
        }
    }

    /// The values now, in the order they were added.
    pub fn values(&self) -> Vec<String> {
        self.values.borrow().clone()
    }
}

#[async_trait]
impl Dns01 for ChallengeTxt {
    async fn add_txt(&mut self, name: &str, value: &str) -> Result<String> {
        if name.trim_end_matches('.').to_ascii_lowercase() != self.name {
            bail!(
                "the subzone's server answers DNS-01 at {} only, not {name}",
                self.name
            );
        }
        self.values.send_modify(|v| v.push(value.to_string()));
        Ok(value.to_string())
    }
    async fn delete(&mut self, id: &str) -> Result<()> {
        self.values.send_modify(|v| {
            if let Some(i) = v.iter().position(|x| x == id) {
                v.remove(i);
            }
        });
        Ok(())
    }
}

/// What `--tls-scope` makes of the coordinator's certificate and subzone arguments.
pub struct TlsPlan {
    /// The renewal, with --tls-secret.
    pub renewal: Option<Renewal>,
    /// What the subzone answers at `_acme-challenge.<subzone>`, with --dns-subzone.
    pub acme: super::authority::Acme,
    /// Where a subzone certificate's renewal writes its DNS-01 records; `None` for a zone
    /// certificate, whose records go through the zone's Cloudflare token.
    pub challenge: Option<ChallengeTxt>,
    /// The zone of the EC2 tiles' health name (`ec2-<n>.<zone>`): a name the certificate
    /// covers.
    pub tile_zone: String,
}

/// The certificate, challenge and tile health zone for the coordinator's arguments.
pub fn plan(args: &crate::cli::FleetCoordArgs) -> Result<TlsPlan> {
    use super::authority::Acme;
    let parent = args.dns_zone.trim_end_matches('.').to_ascii_lowercase();
    let sub = args
        .dns_subzone
        .as_deref()
        .map(|s| s.trim_end_matches('.').to_ascii_lowercase());
    match args.tls_scope {
        TlsScope::Zone => {
            if args.tls_secret.is_some() && args.dns_token_secret.is_none() {
                bail!("--tls-secret needs --dns-token-secret (DNS-01 challenges) with --tls-scope zone");
            }
            Ok(TlsPlan {
                renewal: args
                    .tls_secret
                    .clone()
                    .map(|secret| Renewal::zone(secret, &parent, sub.as_deref())),
                // The subzone's challenge records live in the parent zone, at the name the
                // parent's own challenge uses.
                acme: match &sub {
                    Some(_) => Acme::Cname(format!("_acme-challenge.{parent}")),
                    None => Acme::None,
                },
                challenge: None,
                tile_zone: parent,
            })
        }
        TlsScope::Subzone => {
            let Some(sub) = sub else {
                bail!("--tls-scope subzone needs --dns-subzone");
            };
            if args.dns_token_secret.is_some() {
                bail!(
                    "--tls-scope subzone answers DNS-01 from the subzone's own server and holds \
                     no Cloudflare token: drop --dns-token-secret"
                );
            }
            let txt = ChallengeTxt::new(&format!("_acme-challenge.{sub}"));
            Ok(TlsPlan {
                renewal: args
                    .tls_secret
                    .clone()
                    .map(|secret| Renewal::subzone(secret, &sub)),
                acme: Acme::Txt(txt.clone()),
                challenge: Some(txt),
                tile_zone: sub,
            })
        }
    }
}

/// The identifiers an order for `r` asks for: its domains, nothing else.
pub fn order_identifiers(r: &Renewal) -> Vec<Identifier> {
    r.domains.iter().cloned().map(Identifier::Dns).collect()
}

/// Order a new certificate for `r.domains`, answering DNS-01 at `r.challenge` through `dns`.
pub async fn issue(r: &Renewal, dns: &mut dyn Dns01) -> Result<CertDoc> {
    let (account, _) = Account::builder()?
        .create(
            &NewAccount {
                contact: &[],
                terms_of_service_agreed: true,
                only_return_existing: false,
            },
            r.directory.clone(),
            None,
        )
        .await
        .context("creating the ACME account")?;
    let ids = order_identifiers(r);
    let mut order = account
        .new_order(&NewOrder::new(&ids))
        .await
        .context("ordering the certificate")?;
    let name = r.challenge.clone();
    let mut records = Vec::new();
    let mut values = Vec::new();
    let result = async {
        {
            let mut auths = order.authorizations();
            while let Some(a) = auths.next().await {
                let mut a = a?;
                if a.status == AuthorizationStatus::Valid {
                    continue;
                }
                let ch = a
                    .challenge(ChallengeType::Dns01)
                    .context("the CA offered no DNS-01 challenge")?;
                let value = ch.key_authorization().dns_value();
                records.push(dns.add_txt(&name, &value).await?);
                values.push(value);
            }
        }
        propagated(&r.doh, &name, &values).await?;
        {
            let mut auths = order.authorizations();
            while let Some(a) = auths.next().await {
                let mut a = a?;
                if a.status == AuthorizationStatus::Valid {
                    continue;
                }
                a.challenge(ChallengeType::Dns01)
                    .context("the CA offered no DNS-01 challenge")?
                    .set_ready()
                    .await?;
            }
        }
        let policy = RetryPolicy::new()
            .initial_delay(Duration::from_secs(2))
            .timeout(Duration::from_secs(300));
        let status = order.poll_ready(&policy).await?;
        if status != OrderStatus::Ready {
            bail!("the order is {status:?}, not ready");
        }
        let key = order.finalize().await?;
        let chain = order.poll_certificate(&policy).await?;
        doc_from_pem(&chain, &key, jiff::Timestamp::now())
    }
    .await;
    for id in records {
        if let Err(e) = dns.delete(&id).await {
            tracing::warn!(error = %format!("{e:#}"), "an ACME challenge record was not removed");
        }
    }
    result
}

/// A renewed document with the scene token: the one the secret has as the new version is
/// stored, else the one the coordinator holds (a secret that could not be read or parsed).
pub fn with_scene_token(mut doc: CertDoc, r: &Renewal, current: Option<&str>) -> CertDoc {
    doc.scene_token = current
        .and_then(super::scene_token::from_secret)
        .or_else(|| r.scene_token.clone());
    doc
}

/// Check the certificate twice a day and renew it within 30 days of expiry.
pub async fn run(r: Renewal, secrets: Secrets, mut dns: Box<dyn Dns01>) {
    loop {
        let current = secrets.get(&r.secret).await;
        let due = match &current {
            Ok(s) => r.needs_renewal(Some(s), jiff::Timestamp::now()),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "the certificate secret could not be read");
                false
            }
        };
        let mut wait = r.check_every;
        if due {
            tracing::info!(domains = ?r.domains, challenge = %r.challenge, "renewing the wildcard certificate");
            wait = r.retry_after;
            let issued = issue(&r, dns.as_mut()).await;
            // The token as stored now, not when the renewal began: issuance takes minutes.
            let fresh = match &issued {
                Ok(_) => secrets.get(&r.secret).await.ok(),
                Err(_) => None,
            };
            match issued.map(|d| with_scene_token(d, &r, fresh.as_deref())) {
                Ok(doc) => match secrets
                    .put(
                        &r.secret,
                        &serde_json::to_string(&doc).expect("the document serializes"),
                    )
                    .await
                {
                    Ok(()) => {
                        tracing::info!(not_after = %doc.not_after, "certificate renewed");
                        wait = r.check_every;
                    }
                    Err(e) => {
                        tracing::error!(error = %format!("{e:#}"), "the renewed certificate was not stored")
                    }
                },
                Err(e) => tracing::error!(error = %format!("{e:#}"), "certificate renewal failed"),
            }
        }
        tokio::time::sleep(wait).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(not_after: &str, domains: &[&str]) -> String {
        serde_json::json!({"cert": "", "key": "", "domains": domains, "not_after": not_after,
            "issued_at": "2026-08-01T00:00:00Z", "issuer": "CN=Test"})
        .to_string()
    }

    #[test]
    fn a_renewal_keeps_the_scene_token() {
        let pem = doc_from_pem_for_tests();
        let mut r = Renewal::zone("s".into(), "fcvm.example", None);
        let current =
            serde_json::json!({"cert": "", "key": "", "scene_token": "in-secret"}).to_string();
        assert_eq!(
            with_scene_token(pem.clone(), &r, Some(&current))
                .scene_token
                .as_deref(),
            Some("in-secret")
        );
        r.scene_token = Some("held".into());
        assert_eq!(
            with_scene_token(pem.clone(), &r, Some(&current))
                .scene_token
                .as_deref(),
            Some("in-secret"),
            "a token rotated in the secret stays rotated"
        );
        assert_eq!(
            with_scene_token(pem.clone(), &r, Some("not json"))
                .scene_token
                .as_deref(),
            Some("held")
        );
        let stored: serde_json::Value =
            serde_json::to_value(with_scene_token(pem, &r, None)).unwrap();
        assert_eq!(stored["scene_token"], "held", "stored in the new version");
    }

    /// A renewal takes the token from the secret as read after issuance, which takes minutes:
    /// the read before it could restore a token replaced meanwhile.
    #[test]
    fn a_renewal_reads_the_token_after_issuance() {
        let src = include_str!("cert.rs");
        let body = &src[src.find("pub async fn run(").unwrap()..];
        let issue = body.find("issue(&r, dns.as_mut()).await").unwrap();
        let read = body
            .find("secrets.get(&r.secret).await.ok()")
            .expect("a read of the secret for the token");
        let keep = body
            .find("with_scene_token(d, &r, fresh.as_deref())")
            .unwrap();
        assert!(issue < read && read < keep, "{issue} {read} {keep}");
    }

    fn doc_from_pem_for_tests() -> CertDoc {
        CertDoc {
            cert: "C".into(),
            key: "K".into(),
            domains: vec![],
            not_after: String::new(),
            issued_at: String::new(),
            issuer: String::new(),
            scene_token: None,
        }
    }

    #[test]
    fn renewal_is_due_within_thirty_days_or_when_the_secret_is_unusable() {
        let now = "2026-10-23T00:00:00Z".parse::<jiff::Timestamp>().unwrap();
        let both = ["*.fcvm.dev", "fcvm.dev"];
        let zone = Renewal::zone("s".into(), "fcvm.dev", None).domains;
        assert!(!needs_renewal(
            Some(&doc("2026-11-22T00:00:01Z", &both)),
            &zone,
            now
        ));
        assert!(needs_renewal(
            Some(&doc("2026-11-22T00:00:00Z", &both)),
            &zone,
            now
        ));
        assert!(needs_renewal(
            Some(&doc("2027-01-01T00:00:00Z", &["*.fcvm.dev"])),
            &zone,
            now
        ));
        assert!(needs_renewal(Some("not json"), &zone, now));
        assert!(needs_renewal(None, &zone, now));
        // A subzone's wildcard the certificate does not cover yet.
        let sub = Renewal::zone("s".into(), "fcvm.dev", Some("tiles.fcvm.dev")).domains;
        assert!(needs_renewal(
            Some(&doc("2027-01-01T00:00:00Z", &both)),
            &sub,
            now
        ));
        assert!(!needs_renewal(
            Some(&doc(
                "2027-01-01T00:00:00Z",
                &["*.fcvm.dev", "fcvm.dev", "*.tiles.fcvm.dev"]
            )),
            &sub,
            now
        ));
        assert!(needs_renewal(Some(&doc("soon", &both)), &zone, now));
    }

    /// A subzone certificate orders `*.<subzone>` and nothing else, and answers its one
    /// challenge at `_acme-challenge.<subzone>`: no name in the parent zone, so neither the
    /// key nor a DNS-01 record touches a name in front of Cloudflare.
    #[test]
    fn a_subzone_certificate_names_only_the_subzone() {
        let r = Renewal::subzone("s".into(), "Tiles.FCVM.example.");
        let ids: Vec<String> = order_identifiers(&r)
            .into_iter()
            .map(|i| match i {
                Identifier::Dns(d) => d,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(ids, vec!["*.tiles.fcvm.example".to_string()]);
        assert_eq!(r.challenge, "_acme-challenge.tiles.fcvm.example");
        // The zone certificate is what it was.
        let z = Renewal::zone("s".into(), "fcvm.example", Some("tiles.fcvm.example"));
        assert_eq!(
            z.domains,
            vec!["*.fcvm.example", "fcvm.example", "*.tiles.fcvm.example"]
        );
        assert_eq!(z.challenge, "_acme-challenge.fcvm.example");
        // A secret that still holds the zone certificate is renewed for the subzone alone,
        // also when that certificate covers the subzone's wildcard among its names: its key is
        // valid for the parent zone (Codex on #1973).
        let now = "2026-10-23T00:00:00Z".parse::<jiff::Timestamp>().unwrap();
        let zone_cert = doc("2027-01-01T00:00:00Z", &["*.fcvm.example", "fcvm.example"]);
        assert!(r.needs_renewal(Some(&zone_cert), now));
        let wider = doc(
            "2027-01-01T00:00:00Z",
            &["*.fcvm.example", "fcvm.example", "*.tiles.fcvm.example"],
        );
        assert!(r.needs_renewal(Some(&wider), now));
        let own = doc("2027-01-01T00:00:00Z", &["*.tiles.fcvm.example"]);
        assert!(!r.needs_renewal(Some(&own), now));
        // A zone certificate still takes a secret that covers more than it asks for.
        assert!(!z.needs_renewal(Some(&wider), now));
    }

    fn coord(extra: &[&str]) -> crate::cli::FleetCoordArgs {
        use crate::cli::{Cli, Commands, FleetCommand};
        use clap::Parser;
        let mut argv = vec!["fcvm", "fleet", "coord", "--listen", "127.0.0.1:0"];
        argv.extend_from_slice(extra);
        match Cli::try_parse_from(argv).expect("parses").cmd {
            Commands::Fleet(f) => match f.cmd {
                FleetCommand::Coord(a) => *a,
                _ => panic!("coord"),
            },
            _ => panic!("fleet"),
        }
    }

    /// `--tls-scope subzone` asks for `*.<subzone>` alone, answers its challenge from the
    /// subzone's server, names the EC2 tiles in the subzone, and refuses a Cloudflare token;
    /// the default scope is the zone certificate it was.
    #[test]
    fn the_tls_scope_decides_the_names_the_challenge_and_the_tile_zone() {
        use crate::fleet::coord::authority::Acme;
        let sub = [
            "--dns-zone",
            "fcvm.example",
            "--dns-subzone",
            "tiles.fcvm.example",
            "--dns-subzone-ns",
            "ns1-tiles.fcvm.example",
            "--dns-listen",
            "127.0.0.1:0",
            "--tls-secret",
            "s",
        ];
        let mut args = sub.to_vec();
        args.extend(["--tls-scope", "subzone"]);
        let p = plan(&coord(&args)).unwrap();
        let r = p.renewal.expect("a renewal");
        assert_eq!(r.domains, vec!["*.tiles.fcvm.example".to_string()]);
        assert_eq!(r.challenge, "_acme-challenge.tiles.fcvm.example");
        assert!(
            matches!(&p.acme, Acme::Txt(t) if t.name == r.challenge),
            "{:?}",
            p.acme
        );
        assert!(p.challenge.is_some());
        assert_eq!(p.tile_zone, "tiles.fcvm.example");
        let mut with_token = args.clone();
        with_token.extend(["--dns-token-secret", "t"]);
        assert!(plan(&coord(&with_token)).is_err());
        assert!(plan(&coord(&["--tls-scope", "subzone"])).is_err());
        // The zone scope, the default.
        let mut zone = sub.to_vec();
        zone.extend(["--dns-token-secret", "t"]);
        let p = plan(&coord(&zone)).unwrap();
        assert_eq!(
            p.renewal.unwrap().domains,
            vec!["*.fcvm.example", "fcvm.example", "*.tiles.fcvm.example"]
        );
        assert_eq!(p.acme, Acme::Cname("_acme-challenge.fcvm.example".into()));
        assert!(p.challenge.is_none());
        assert_eq!(p.tile_zone, "fcvm.example");
        assert!(
            plan(&coord(&sub)).is_err(),
            "a zone certificate needs the token"
        );
    }

    /// The subzone's challenge records are written where its DNS server reads them, and only
    /// at its one name.
    #[tokio::test]
    async fn the_subzone_challenge_holds_its_values_until_they_are_removed() {
        let mut txt = ChallengeTxt::new("_acme-challenge.tiles.fcvm.example");
        let reader = txt.clone();
        let id = txt
            .add_txt("_acme-challenge.tiles.fcvm.example", "v1")
            .await
            .unwrap();
        assert!(txt
            .add_txt("_acme-challenge.fcvm.example", "v2")
            .await
            .is_err());
        assert_eq!(reader.values(), vec!["v1".to_string()]);
        txt.delete(&id).await.unwrap();
        assert!(reader.values().is_empty());
    }

    #[test]
    fn the_document_reads_expiry_names_and_issuer_from_the_leaf() {
        let mut params =
            rcgen::CertificateParams::new(vec!["*.fcvm.example".into(), "fcvm.example".into()])
                .unwrap();
        params.not_after = rcgen::date_time_ymd(2027, 1, 21);
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let now = "2026-10-23T00:00:00Z".parse::<jiff::Timestamp>().unwrap();
        let d = doc_from_pem(&cert.pem(), &key.serialize_pem(), now).unwrap();
        assert_eq!(d.not_after, "2027-01-21T00:00:00Z");
        assert_eq!(d.issued_at, "2026-10-23T00:00:00Z");
        assert_eq!(d.domains, vec!["*.fcvm.example", "fcvm.example"]);
        assert!(d.key.contains("PRIVATE KEY"));
        // A round trip through the stored form, as issue-cert.sh writes it.
        let back: CertDoc = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
        assert_eq!(back, d);
        assert!(!needs_renewal(
            Some(&serde_json::to_string(&d).unwrap()),
            &Renewal::zone("s".into(), "fcvm.example", None).domains,
            now
        ));
    }
}
