//! Cloudflare Access JWTs: the `Cf-Access-Jwt-Assertion` header Access adds to a request it
//! admitted. The coordinator verifies the token itself (RS256 signature against the team's
//! published keys, issuer, audience, expiry) wherever it relies on Access, instead of trusting
//! that the connector in front of it checked the header:
//!
//! - `/live`: a request carrying a valid token for one of `--access-aud` counts as verified
//!   for Turnstile (the owner, and the fleet's CI and smokes through the Access hostname); with
//!   `--access-viewer-address`, only a valid service-token JWT keys a request by its
//!   `x-fleet-viewer-ip`.
//! - `/agent`: with `--agent-access-aud`, an agent's connection needs a valid service-token JWT
//!   for one of those audiences.
//!
//! The keys are read from `https://<team>/cdn-cgi/access/certs` (the JWKS Cloudflare publishes
//! per team), kept for [`KEYS_TTL`], and read again early when a token names a key id the
//! cache does not hold (Cloudflare rotates its signing keys), at most once per
//! [`REFETCH_EVERY`].

use super::admission::b64url_decode;
use anyhow::{bail, Context, Result};
use axum::http::HeaderMap;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// The header Access forwards its token in.
pub const HEADER: &str = "cf-access-jwt-assertion";
/// How long the team's keys are kept before they are read again.
pub const KEYS_TTL: Duration = Duration::from_secs(3600);
/// The shortest time between two reads of the keys.
pub const REFETCH_EVERY: Duration = Duration::from_secs(10);
/// How long one read of the keys may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// The claims the coordinator reads from a verified token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claims {
    /// A person's address; empty for a service token.
    pub email: String,
    /// A service token's client id; empty for a person.
    pub common_name: String,
}

impl Claims {
    /// A service token (the fleet's Worker, CI, agents): it names a client id and no person.
    pub fn service_token(&self) -> bool {
        !self.common_name.is_empty() && self.email.is_empty()
    }
}

/// One RSA public key, as the JWKS gives it (big-endian, no leading zeros).
#[derive(Clone)]
struct Key {
    n: Vec<u8>,
    e: Vec<u8>,
}

#[derive(Default)]
struct Keys {
    by_kid: HashMap<String, Key>,
    /// The last read that succeeded.
    fetched_at: Option<Instant>,
    /// The last read asked for, whether or not it succeeded: reads are this far apart.
    attempted_at: Option<Instant>,
}

/// Verifies Access tokens for one team.
pub struct AccessJwt {
    issuer: String,
    certs_url: String,
    http: reqwest::Client,
    keys: tokio::sync::Mutex<Keys>,
}

/// The issuer of a team's tokens: `https://<team domain>`. Accepts the domain with or without
/// its scheme.
pub fn issuer(team_domain: &str) -> String {
    let d = team_domain
        .trim()
        .trim_end_matches('/')
        .trim_start_matches("https://");
    format!("https://{d}")
}

impl AccessJwt {
    /// For the team at `team_domain` (`<team>.cloudflareaccess.com`).
    pub fn new(team_domain: &str) -> Result<Self> {
        let iss = issuer(team_domain);
        let certs = format!("{iss}/cdn-cgi/access/certs");
        AccessJwt::with_certs_url(&iss, &certs)
    }

    /// With the keys read from `certs_url` (tests).
    pub fn with_certs_url(issuer: &str, certs_url: &str) -> Result<Self> {
        Ok(AccessJwt {
            issuer: issuer.to_string(),
            certs_url: certs_url.to_string(),
            http: reqwest::Client::builder().timeout(FETCH_TIMEOUT).build()?,
            keys: tokio::sync::Mutex::default(),
        })
    }

    /// The token in `headers`, verified for one of `audiences` at `now_s` (unix seconds).
    /// `None` without a header; an error for a token that does not verify.
    pub async fn from_headers(
        &self,
        headers: &HeaderMap,
        audiences: &[String],
        now_s: u64,
    ) -> Option<Result<Claims>> {
        let jwt = headers.get(HEADER)?.to_str().ok()?;
        Some(self.verify(jwt, audiences, now_s).await)
    }

    /// Verify `jwt` for one of `audiences` at `now_s` (unix seconds).
    pub async fn verify(&self, jwt: &str, audiences: &[String], now_s: u64) -> Result<Claims> {
        let mut parts = jwt.split('.');
        let (Some(h), Some(p), Some(s), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            bail!("an Access token has three parts");
        };
        let header: serde_json::Value = serde_json::from_slice(
            &b64url_decode(h).context("the token's header is not base64url")?,
        )
        .context("the token's header is not JSON")?;
        if header.get("alg").and_then(|a| a.as_str()) != Some("RS256") {
            bail!("the token is not RS256");
        }
        let kid = header
            .get("kid")
            .and_then(|k| k.as_str())
            .context("the token names no key")?;
        let sig = b64url_decode(s).context("the token's signature is not base64url")?;
        let key = self.key(kid).await?;
        ring::signature::RsaPublicKeyComponents {
            n: &key.n,
            e: &key.e,
        }
        .verify(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            format!("{h}.{p}").as_bytes(),
            &sig,
        )
        .map_err(|_| anyhow::anyhow!("the token's signature does not verify"))?;
        let claims: serde_json::Value = serde_json::from_slice(
            &b64url_decode(p).context("the token's payload is not base64url")?,
        )
        .context("the token's payload is not JSON")?;
        if claims.get("iss").and_then(|i| i.as_str()) != Some(self.issuer.as_str()) {
            bail!("the token was issued by another team");
        }
        let aud: Vec<&str> = match claims.get("aud") {
            Some(serde_json::Value::String(a)) => vec![a.as_str()],
            Some(serde_json::Value::Array(a)) => a.iter().filter_map(|x| x.as_str()).collect(),
            _ => vec![],
        };
        if !aud.iter().any(|a| audiences.iter().any(|w| w == a)) {
            bail!("the token is for another application");
        }
        let exp = claims
            .get("exp")
            .and_then(|e| e.as_u64())
            .context("the token has no expiry")?;
        if exp <= now_s {
            bail!("the token expired");
        }
        if claims
            .get("nbf")
            .and_then(|n| n.as_u64())
            .is_some_and(|nbf| nbf > now_s + 60)
        {
            bail!("the token is not valid yet");
        }
        let text = |k: &str| {
            claims
                .get(k)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        Ok(Claims {
            email: text("email"),
            common_name: text("common_name"),
        })
    }

    /// The key `kid`, reading the team's keys again when they are stale or do not hold it.
    async fn key(&self, kid: &str) -> Result<Key> {
        let mut keys = self.keys.lock().await;
        let fresh = keys.fetched_at.is_some_and(|t| t.elapsed() < KEYS_TTL);
        if fresh {
            if let Some(k) = keys.by_kid.get(kid) {
                return Ok(k.clone());
            }
        }
        let may_fetch = keys
            .attempted_at
            .is_none_or(|t| t.elapsed() >= REFETCH_EVERY);
        if may_fetch {
            keys.attempted_at = Some(Instant::now());
            match self.fetch().await {
                Ok(by_kid) => {
                    keys.by_kid = by_kid;
                    keys.fetched_at = Some(Instant::now());
                }
                Err(e) if keys.by_kid.is_empty() => return Err(e),
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "reading the Access keys failed; keeping the last ones")
                }
            }
        }
        keys.by_kid
            .get(kid)
            .cloned()
            .with_context(|| format!("no Access key {kid}"))
    }

    async fn fetch(&self) -> Result<HashMap<String, Key>> {
        let v: serde_json::Value = self
            .http
            .get(&self.certs_url)
            .send()
            .await
            .context("reading the Access keys")?
            .error_for_status()
            .context("reading the Access keys")?
            .json()
            .await
            .context("the Access keys are not JSON")?;
        let mut out = HashMap::new();
        for k in v
            .get("keys")
            .and_then(|k| k.as_array())
            .context("the Access keys list no keys")?
        {
            let field = |name: &str| k.get(name).and_then(|x| x.as_str());
            if field("kty") != Some("RSA") {
                continue;
            }
            let (Some(kid), Some(n), Some(e)) = (field("kid"), field("n"), field("e")) else {
                continue;
            };
            let (Some(n), Some(e)) = (b64url_decode(n), b64url_decode(e)) else {
                continue;
            };
            out.insert(kid.to_string(), Key { n, e });
        }
        if out.is_empty() {
            bail!("the Access keys hold no RSA key");
        }
        Ok(out)
    }
}

/// Where the coordinator checks Access tokens, and for which applications.
pub struct AccessCheck {
    pub jwt: AccessJwt,
    /// The audiences (Access application tags) a `/live` token may be for.
    pub live: Vec<String>,
    /// The audiences an `/agent` token must be for; empty: `/agent` is not checked here.
    pub agent: Vec<String>,
}

fn now_s() -> u64 {
    crate::fleet::now_ms() / 1000
}

impl AccessCheck {
    /// The claims of a `/live` request's token, when it carries one that verifies for a
    /// `/live` audience.
    pub async fn live_claims(&self, headers: &HeaderMap) -> Option<Claims> {
        match self.jwt.from_headers(headers, &self.live, now_s()).await? {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::debug!(error = %format!("{e:#}"), "an Access token on /live does not verify");
                None
            }
        }
    }

    /// An `/agent` request's token: present, valid for an agent audience, and a service token.
    pub async fn agent_claims(&self, headers: &HeaderMap) -> Result<Claims> {
        let c = self
            .jwt
            .from_headers(headers, &self.agent, now_s())
            .await
            .context("no Access token")??;
        if !c.service_token() {
            bail!("the Access token is not a service token");
        }
        Ok(c)
    }
}

/// A `/live` request carrying a valid token for a `/live` audience is verified (Turnstile):
/// the owner, and the fleet's CI and smokes through the Access-protected hostname.
#[async_trait::async_trait]
impl super::turnstile::RequestProof for AccessCheck {
    async fn verified(&self, headers: &HeaderMap) -> bool {
        self.live_claims(headers).await.is_some()
    }
}

#[cfg(test)]
#[path = "access_jwt_testing.rs"]
pub mod testing;

#[cfg(test)]
mod tests {
    use super::testing::{b64url, serve_failing_keys, serve_keys, TestSigner};
    use super::*;

    pub const ISS: &str = "https://team.example.cloudflareaccess.com";
    pub const AUD: &str = "aud-live-for-tests";
    const NOW: u64 = 1_800_000_000;

    fn claims(over: serde_json::Value) -> serde_json::Value {
        let mut c = serde_json::json!({"iss": ISS, "aud": [AUD], "exp": NOW + 600, "iat": NOW - 10,
            "type": "app", "common_name": "client-id.access", "email": ""});
        for (k, v) in over.as_object().unwrap() {
            c[k] = v.clone();
        }
        c
    }

    async fn verifier() -> (
        AccessJwt,
        TestSigner,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let s = TestSigner::new(false, "kid-1");
        let (url, reads) = serve_keys(vec![s.jwk()]).await;
        (AccessJwt::with_certs_url(ISS, &url).unwrap(), s, reads)
    }

    #[tokio::test]
    async fn a_valid_token_verifies_and_names_its_service_token() {
        let (v, s, _) = verifier().await;
        let c = v
            .verify(&s.token(&claims(serde_json::json!({}))), &[AUD.into()], NOW)
            .await
            .unwrap();
        assert!(c.service_token());
        let person = v
            .verify(
                &s.token(&claims(
                    serde_json::json!({"email": "owner@example.test", "common_name": ""}),
                )),
                &[AUD.into()],
                NOW,
            )
            .await
            .unwrap();
        assert!(!person.service_token());
        // A single audience as a string is read too.
        assert!(v
            .verify(
                &s.token(&claims(serde_json::json!({"aud": AUD}))),
                &[AUD.into()],
                NOW
            )
            .await
            .is_ok());
    }

    /// A token signed by another key, one whose payload was changed after signing, and one
    /// with no signature are refused.
    #[tokio::test]
    async fn a_forged_token_is_refused() {
        let (v, s, _) = verifier().await;
        let other = TestSigner::new(true, "kid-1");
        let forged = other.token(&claims(serde_json::json!({})));
        let e = v.verify(&forged, &[AUD.into()], NOW).await.unwrap_err();
        assert!(format!("{e:#}").contains("signature"), "{e:#}");
        let good = s.token(&claims(serde_json::json!({})));
        let mut parts: Vec<&str> = good.split('.').collect();
        let swapped = b64url(
            claims(serde_json::json!({"common_name": "someone-else.access"}))
                .to_string()
                .as_bytes(),
        );
        parts[1] = &swapped;
        assert!(v
            .verify(&parts.join("."), &[AUD.into()], NOW)
            .await
            .is_err());
        let unsigned = format!(
            "{}.{}.",
            b64url(br#"{"alg":"none","kid":"kid-1"}"#),
            b64url(claims(serde_json::json!({})).to_string().as_bytes())
        );
        assert!(v.verify(&unsigned, &[AUD.into()], NOW).await.is_err());
        assert!(v.verify("not-a-token", &[AUD.into()], NOW).await.is_err());
    }

    #[tokio::test]
    async fn a_token_for_another_application_or_team_is_refused() {
        let (v, s, _) = verifier().await;
        let e = v
            .verify(
                &s.token(&claims(serde_json::json!({"aud": ["another-app"]}))),
                &[AUD.into()],
                NOW,
            )
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("another application"), "{e:#}");
        let e = v
            .verify(
                &s.token(&claims(
                    serde_json::json!({"iss": "https://other.cloudflareaccess.com"}),
                )),
                &[AUD.into()],
                NOW,
            )
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("another team"), "{e:#}");
        // No audience configured accepts nothing.
        assert!(v
            .verify(&s.token(&claims(serde_json::json!({}))), &[], NOW)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn an_expired_token_is_refused() {
        let (v, s, _) = verifier().await;
        let t = s.token(&claims(serde_json::json!({"exp": NOW})));
        let e = v.verify(&t, &[AUD.into()], NOW).await.unwrap_err();
        assert!(format!("{e:#}").contains("expired"), "{e:#}");
        assert!(v.verify(&t, &[AUD.into()], NOW - 1).await.is_ok());
        let no_exp = s.token(&serde_json::json!({"iss": ISS, "aud": [AUD]}));
        assert!(v.verify(&no_exp, &[AUD.into()], NOW).await.is_err());
    }

    /// The keys are read once and kept; a token naming a key the cache does not hold reads
    /// them again, at most once per REFETCH_EVERY.
    #[tokio::test]
    async fn keys_are_cached_and_an_unknown_key_reads_them_again_at_most_so_often() {
        let (v, s, reads) = verifier().await;
        for _ in 0..3 {
            v.verify(&s.token(&claims(serde_json::json!({}))), &[AUD.into()], NOW)
                .await
                .unwrap();
        }
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 1);
        let rotated = TestSigner::new(true, "kid-2");
        for _ in 0..3 {
            assert!(v
                .verify(
                    &rotated.token(&claims(serde_json::json!({}))),
                    &[AUD.into()],
                    NOW
                )
                .await
                .is_err());
        }
        assert_eq!(
            reads.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "within REFETCH_EVERY"
        );
    }

    /// A keys endpoint that fails is asked at most once per REFETCH_EVERY too, so a failing
    /// endpoint does not make every handshake wait for another read.
    #[tokio::test]
    async fn a_failing_keys_endpoint_is_not_asked_again_on_every_token() {
        let s = TestSigner::new(false, "kid-1");
        let (url, reads) = serve_failing_keys().await;
        let v = AccessJwt::with_certs_url(ISS, &url).unwrap();
        for _ in 0..3 {
            assert!(v
                .verify(&s.token(&claims(serde_json::json!({}))), &[AUD.into()], NOW)
                .await
                .is_err());
        }
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn the_issuer_is_the_team_domain_over_https() {
        assert_eq!(issuer("team.example.cloudflareaccess.com"), ISS);
        assert_eq!(issuer("https://team.example.cloudflareaccess.com/"), ISS);
    }
}
