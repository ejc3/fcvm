//! Cloudflare Turnstile: a `/live` connection proves it is a browser with a person behind it
//! before its first intent. The page runs the widget and sends its token
//! (`{"type":"verify","token"}`); the coordinator asks Cloudflare's siteverify with its
//! secret. Without a secret configured (local runs) every connection counts as verified.

use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::BTreeSet;
use std::time::Duration;

pub const SITEVERIFY: &str = "https://challenges.cloudflare.com/turnstile/v0/siteverify";

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
        Ok(Siteverify {
            secret,
            url: url.unwrap_or_else(|| SITEVERIFY.to_string()),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()?,
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
