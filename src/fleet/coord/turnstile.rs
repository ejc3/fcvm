//! Cloudflare Turnstile: a `/live` connection proves it is a browser with a person behind it
//! before it counts as a viewer, takes a seat or sends an intent. The page runs the widget and
//! sends its token (`{"type":"verify","token"}`); the coordinator asks Cloudflare's siteverify
//! with its secret. Without a secret configured (staging, local runs, tests) every connection
//! counts as verified.
//!
//! A request can also prove itself before any token ([`RequestProof`]): one that carries a valid
//! Cloudflare Access JWT came from the owner or the fleet's own CI.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use axum::http::HeaderMap;
use std::collections::BTreeSet;
use std::time::Duration;

pub const SITEVERIFY: &str = "https://challenges.cloudflare.com/turnstile/v0/siteverify";
/// How long one siteverify call may take. Past it the connection is not verified (it fails
/// closed) and the page offers a retry.
pub const SITEVERIFY_TIMEOUT: Duration = Duration::from_secs(10);
/// The log marker of a siteverify call that failed (not a refusal: an error or a timeout).
pub const SITEVERIFY_ERROR_MARKER: &str = "turnstile-siteverify-error";

/// The keys of the Turnstile secret in Secrets Manager (`--turnstile-secret-id`), the same
/// names as the environment variables the flags read.
pub const SECRET_SITE_KEY: &str = "FLEET_TURNSTILE_SITE_KEY";
pub const SECRET_SECRET: &str = "FLEET_TURNSTILE_SECRET";

/// The site key and the secret from the Secrets Manager secret's JSON.
pub fn parse_secret(json: &str) -> Result<(String, String)> {
    let v: serde_json::Value =
        serde_json::from_str(json).context("the Turnstile secret is not JSON")?;
    let field = |k: &str| {
        v.get(k)
            .and_then(|s| s.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    match (field(SECRET_SITE_KEY), field(SECRET_SECRET)) {
        (Some(site), Some(secret)) => Ok((site, secret)),
        _ => bail!("the Turnstile secret needs the keys {SECRET_SITE_KEY} and {SECRET_SECRET}"),
    }
}

/// What a `/live` request's headers prove about its viewer before any Turnstile token.
#[async_trait]
pub trait RequestProof: Send + Sync {
    /// Whether the request counts as verified as it is.
    async fn verified(&self, headers: &HeaderMap) -> bool;
}

/// Nothing in a request proves its viewer.
pub struct NoProof;

#[async_trait]
impl RequestProof for NoProof {
    async fn verified(&self, _headers: &HeaderMap) -> bool {
        false
    }
}

#[async_trait]
pub trait Verifier: Send + Sync {
    /// Whether verification is asked for at all.
    fn required(&self) -> bool {
        true
    }
    /// `Ok(None)` when Cloudflare accepted the token; `Ok(Some(reason))` when it refused.
    async fn verify(&self, token: &str, remote_ip: &str) -> Result<Option<String>>;
}

/// No secret configured: nothing to verify.
pub struct Off;

#[async_trait]
impl Verifier for Off {
    fn required(&self) -> bool {
        false
    }
    async fn verify(&self, _token: &str, _remote_ip: &str) -> Result<Option<String>> {
        Ok(None)
    }
}

/// Cloudflare's siteverify.
pub struct Siteverify {
    secret: String,
    url: String,
    http: reqwest::Client,
}

impl Siteverify {
    pub fn new(secret: String, url: Option<String>) -> Result<Self> {
        Siteverify::with_timeout(secret, url, SITEVERIFY_TIMEOUT)
    }

    pub fn with_timeout(secret: String, url: Option<String>, timeout: Duration) -> Result<Self> {
        Ok(Siteverify {
            secret,
            url: url.unwrap_or_else(|| SITEVERIFY.to_string()),
            http: reqwest::Client::builder().timeout(timeout).build()?,
        })
    }
}

#[async_trait]
impl Verifier for Siteverify {
    async fn verify(&self, token: &str, remote_ip: &str) -> Result<Option<String>> {
        let r: serde_json::Value = self
            .http
            .post(&self.url)
            .form(&[
                ("secret", self.secret.as_str()),
                ("response", token),
                ("remoteip", remote_ip),
            ])
            .send()
            .await
            .context("calling Turnstile siteverify")?
            .error_for_status()
            .context("Turnstile siteverify answered with an error")?
            .json()
            .await
            .context("reading Turnstile's answer")?;
        if r.get("success").and_then(|s| s.as_bool()) == Some(true) {
            return Ok(None);
        }
        let codes = r
            .get("error-codes")
            .map(|c| c.to_string())
            .unwrap_or_else(|| "no error codes".into());
        Ok(Some(format!("Turnstile refused the token: {codes}")))
    }
}

/// A verifier for tests: accepts exactly the listed tokens.
pub struct Fake {
    pub accept: BTreeSet<String>,
}

#[async_trait]
impl Verifier for Fake {
    async fn verify(&self, token: &str, _remote_ip: &str) -> Result<Option<String>> {
        Ok((!self.accept.contains(token)).then(|| "the fake verifier refused the token".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post;

    #[test]
    fn the_secret_names_its_site_key_and_secret() {
        let (site, secret) = parse_secret(
            r#"{"FLEET_TURNSTILE_SITE_KEY": "0x-site", "FLEET_TURNSTILE_SECRET": "0x-secret"}"#,
        )
        .unwrap();
        assert_eq!((site.as_str(), secret.as_str()), ("0x-site", "0x-secret"));
        for bad in [
            "not json",
            r#"{"FLEET_TURNSTILE_SITE_KEY": "0x-site"}"#,
            r#"{"FLEET_TURNSTILE_SITE_KEY": "0x-site", "FLEET_TURNSTILE_SECRET": " "}"#,
            r#"{"FLEET_TURNSTILE_SITE_KEY": 1, "FLEET_TURNSTILE_SECRET": "0x-secret"}"#,
        ] {
            let e = parse_secret(bad).unwrap_err();
            assert!(!format!("{e:#}").contains("0x-secret"), "{e:#}");
        }
    }

    #[tokio::test]
    async fn siteverify_posts_the_secret_token_and_address() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let s2 = seen.clone();
        let app = axum::Router::new().route(
            "/siteverify",
            post(move |body: String| {
                let s2 = s2.clone();
                async move {
                    *s2.lock().unwrap() = body.clone();
                    let ok = body.contains("response=good");
                    axum::Json(serde_json::json!({"success": ok, "error-codes": if ok { vec![] } else { vec!["invalid-input-response"] }}))
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/siteverify", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let v = Siteverify::new("s3cret".into(), Some(url)).unwrap();
        assert_eq!(v.verify("good", "198.51.100.1").await.unwrap(), None);
        let body = seen.lock().unwrap().clone();
        assert!(
            body.contains("secret=s3cret") && body.contains("remoteip=198.51.100.1"),
            "{body}"
        );
        let refused = v.verify("bad", "198.51.100.1").await.unwrap().unwrap();
        assert!(refused.contains("invalid-input-response"), "{refused}");
    }
}
