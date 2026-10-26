//! AWS Secrets Manager, read and written with the task's or instance's role. Secret values
//! stay in memory: they are never logged and never written anywhere but where a caller
//! puts them.

use super::aws::{signed, CredentialChain};
use anyhow::{bail, Context, Result};
use std::time::Duration;

pub struct Secrets {
    chain: CredentialChain,
    http: reqwest::Client,
    region: String,
    base: String,
}

impl Secrets {
    pub fn new(region: &str) -> Result<Self> {
        Secrets::with_endpoint(
            region,
            &format!("https://secretsmanager.{region}.amazonaws.com"),
        )
    }

    /// Against another endpoint (tests).
    pub fn with_endpoint(region: &str, base: &str) -> Result<Self> {
        Ok(Secrets {
            chain: CredentialChain::new()?,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()?,
            region: region.into(),
            base: base.into(),
        })
    }

    async fn call(&self, target: &str, body: serde_json::Value) -> Result<serde_json::Value> {
        let (status, bytes) = signed(
            &self.chain,
            &self.http,
            &self.base,
            reqwest::Method::POST,
            "/",
            "",
            &self.region,
            "secretsmanager",
            &[
                ("x-amz-target", format!("secretsmanager.{target}")),
                ("content-type", "application/x-amz-json-1.1".into()),
            ],
            serde_json::to_vec(&body)?,
        )
        .await?;
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
        if !status.is_success() {
            // The error's type and message only: a request body is never echoed.
            bail!(
                "Secrets Manager {target} answered {status}: {} {}",
                v.get("__type").and_then(|t| t.as_str()).unwrap_or(""),
                v.get("message")
                    .or_else(|| v.get("Message"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
            );
        }
        Ok(v)
    }

    /// The secret's current string.
    pub async fn get(&self, id: &str) -> Result<String> {
        let v = self
            .call("GetSecretValue", serde_json::json!({ "SecretId": id }))
            .await?;
        v.get("SecretString")
            .and_then(|s| s.as_str())
            .map(str::to_string)
            .with_context(|| format!("secret {id} has no string value"))
    }

    /// Store a new current version of the secret. The API requires a `ClientRequestToken`
    /// (the SDKs fill it in; this client calls the API directly): a fresh one per call makes
    /// each call a new version, and a retried request with the same token is not stored twice.
    pub async fn put(&self, id: &str, value: &str) -> Result<()> {
        self.call(
            "PutSecretValue",
            serde_json::json!({
                "SecretId": id,
                "SecretString": value,
                "ClientRequestToken": uuid::Uuid::new_v4().to_string(),
            }),
        )
        .await
        .map(|_| ())
    }
}
