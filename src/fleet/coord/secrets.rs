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

    /// Store a new current version of the secret.
    pub async fn put(&self, id: &str, value: &str) -> Result<()> {
        self.call("PutSecretValue", Self::put_body(id, value))
            .await
            .map(|_| ())
    }

    fn put_body(id: &str, value: &str) -> serde_json::Value {
        // Raw API calls do not get the SDK's automatic token. Build it once per
        // write so resending this request keeps the same secret version.
        serde_json::json!({
            "SecretId": id,
            "SecretString": value,
            "ClientRequestToken": uuid::Uuid::new_v4().to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_put_request_has_a_fresh_uuid_for_each_secret_version() {
        let body = Secrets::put_body("example-secret", "example-value");
        let wire = serde_json::to_vec(&body).unwrap();
        let request: serde_json::Value = serde_json::from_slice(&wire).unwrap();
        assert_eq!(request["SecretId"], "example-secret");
        assert_eq!(request["SecretString"], "example-value");
        let token = request["ClientRequestToken"]
            .as_str()
            .expect("raw PutSecretValue requests require ClientRequestToken");
        assert!((32..=64).contains(&token.len()));
        assert_eq!(uuid::Uuid::parse_str(token).unwrap().get_version_num(), 4);
        let next = Secrets::put_body("example-secret", "example-value");
        assert_ne!(next["ClientRequestToken"], token);
    }
}
