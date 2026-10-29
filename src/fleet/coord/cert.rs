//! Renewal of the fleet's wildcard certificate (`*.<zone>` and `<zone>`), kept in a Secrets
//! Manager secret in the format fleet/tools/issue-cert.sh writes: a JSON object with
//! `cert` (PEM full chain), `key` (PEM private key), `domains`, `not_after`, `issued_at`
//! (RFC 3339 UTC) and `issuer`. Twice a day the coordinator reads it; within 30 days of
//! expiry it orders a new certificate from the ACME directory (Let's Encrypt), answers
//! DNS-01 with TXT records through the zone's DNS token, stores the result as the secret's
//! new version and removes the TXT records. The key and certificate stay in memory.

use super::dns::Cloudflare;
use super::secrets::Secrets;
use anyhow::{bail, Context, Result};
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
pub const RENEW_BEFORE: chrono::Duration = chrono::Duration::days(30);

/// The names the certificate covers: the zone, its wildcard and `extra`.
pub fn domains(zone: &str, extra: &[String]) -> Vec<String> {
    let mut d = vec![format!("*.{zone}"), zone.to_string()];
    d.extend(extra.iter().cloned());
    d
}

/// Whether the stored secret needs a new certificate now: missing, unreadable, not
/// covering every name of [`domains`], or expiring within `RENEW_BEFORE`.
pub fn needs_renewal(
    secret: Option<&str>,
    zone: &str,
    extra: &[String],
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    let Some(doc) = secret.and_then(|s| serde_json::from_str::<CertDoc>(s).ok()) else {
        return true;
    };
    if !domains(zone, extra)
        .iter()
        .all(|d| doc.domains.iter().any(|x| x == d))
    {
        return true;
    }
    match chrono::DateTime::parse_from_rfc3339(&doc.not_after) {
        Ok(t) => t.with_timezone(&chrono::Utc) - now <= RENEW_BEFORE,
        Err(_) => true,
    }
}

/// The secret's document for a PEM chain and key: expiry, names and issuer read from the
/// leaf certificate.
pub fn doc_from_pem(chain: &str, key: &str, now: chrono::DateTime<chrono::Utc>) -> Result<CertDoc> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(chain.as_bytes())
        .map_err(|e| anyhow::anyhow!("the chain is not PEM: {e}"))?;
    let leaf = pem
        .parse_x509()
        .map_err(|e| anyhow::anyhow!("the leaf is not X.509: {e}"))?;
    let not_after = chrono::DateTime::from_timestamp(leaf.validity().not_after.timestamp(), 0)
        .context("the leaf's expiry")?;
    let mut domains = Vec::new();
    if let Ok(Some(san)) = leaf.subject_alternative_name() {
        for n in &san.value.general_names {
            if let x509_parser::extensions::GeneralName::DNSName(d) = n {
                domains.push(d.to_string());
            }
        }
    }
    let fmt = |t: chrono::DateTime<chrono::Utc>| t.format("%Y-%m-%dT%H:%M:%SZ").to_string();
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
    pub zone: String,
    /// Names beyond the zone and its wildcard: the launch subzone's wildcard, whose
    /// `_acme-challenge` the subzone's server answers with a CNAME to the zone's own, so
    /// every challenge record is written at `_acme-challenge.<zone>`.
    pub extra: Vec<String>,
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
    pub fn new(secret: String, zone: String) -> Self {
        Renewal {
            secret,
            zone,
            extra: Vec::new(),
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

/// Order a new certificate for the zone and its wildcard, answering DNS-01 through `cf`.
pub async fn issue(r: &Renewal, cf: &mut Cloudflare) -> Result<CertDoc> {
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
    let ids: Vec<Identifier> = domains(&r.zone, &r.extra)
        .into_iter()
        .map(Identifier::Dns)
        .collect();
    let mut order = account
        .new_order(&NewOrder::new(&ids))
        .await
        .context("ordering the certificate")?;
    let name = format!("_acme-challenge.{}", r.zone);
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
                records.push(cf.add_txt(&name, &value).await?);
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
        doc_from_pem(&chain, &key, chrono::Utc::now())
    }
    .await;
    for id in records {
        if let Err(e) = cf.delete(&id).await {
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
pub async fn run(r: Renewal, secrets: Secrets, mut cf: Cloudflare) {
    loop {
        let current = secrets.get(&r.secret).await;
        let due = match &current {
            Ok(s) => needs_renewal(Some(s), &r.zone, &r.extra, chrono::Utc::now()),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "the certificate secret could not be read");
                false
            }
        };
        let mut wait = r.check_every;
        if due {
            tracing::info!(zone = %r.zone, extra = ?r.extra, "renewing the wildcard certificate");
            wait = r.retry_after;
            let issued = issue(&r, &mut cf).await;
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
        let mut r = Renewal::new("s".into(), "fcvm.example".into());
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
        let issue = body.find("issue(&r, &mut cf).await").unwrap();
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
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-23T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let both = ["*.fcvm.dev", "fcvm.dev"];
        assert!(!needs_renewal(
            Some(&doc("2026-11-22T00:00:01Z", &both)),
            "fcvm.dev",
            &[],
            now
        ));
        assert!(needs_renewal(
            Some(&doc("2026-11-22T00:00:00Z", &both)),
            "fcvm.dev",
            &[],
            now
        ));
        assert!(needs_renewal(
            Some(&doc("2027-01-01T00:00:00Z", &["*.fcvm.dev"])),
            "fcvm.dev",
            &[],
            now
        ));
        assert!(needs_renewal(Some("not json"), "fcvm.dev", &[], now));
        assert!(needs_renewal(None, "fcvm.dev", &[], now));
        // A subzone's wildcard the certificate does not cover yet.
        let sub = ["*.tiles.fcvm.dev".to_string()];
        assert!(needs_renewal(
            Some(&doc("2027-01-01T00:00:00Z", &both)),
            "fcvm.dev",
            &sub,
            now
        ));
        assert!(!needs_renewal(
            Some(&doc(
                "2027-01-01T00:00:00Z",
                &["*.fcvm.dev", "fcvm.dev", "*.tiles.fcvm.dev"]
            )),
            "fcvm.dev",
            &sub,
            now
        ));
        assert!(needs_renewal(
            Some(&doc("soon", &both)),
            "fcvm.dev",
            &[],
            now
        ));
    }

    #[test]
    fn the_document_reads_expiry_names_and_issuer_from_the_leaf() {
        let mut params =
            rcgen::CertificateParams::new(vec!["*.fcvm.example".into(), "fcvm.example".into()])
                .unwrap();
        params.not_after = rcgen::date_time_ymd(2027, 1, 21);
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-23T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
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
            "fcvm.example",
            &[],
            now
        ));
    }
}
