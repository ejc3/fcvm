//! AWS credentials for the Bedrock text check, in the order the AWS SDKs look:
//! environment variables, the ECS task role endpoint, then the EC2 instance role (IMDSv2).
//! Role credentials are cached until five minutes before they expire.

use super::sigv4::Credentials;
use anyhow::{bail, Context, Result};
use std::time::Duration;
use tokio::sync::Mutex;

#[derive(Clone)]
struct Cached {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
    expires: Option<chrono::DateTime<chrono::Utc>>,
}

impl Cached {
    fn fresh(&self) -> bool {
        self.expires
            .is_none_or(|e| e - chrono::Duration::minutes(5) > chrono::Utc::now())
    }
    fn creds(&self) -> Credentials {
        Credentials {
            access_key_id: self.access_key_id.clone(),
            secret_access_key: self.secret_access_key.clone(),
            session_token: self.session_token.clone(),
        }
    }
}

pub struct CredentialChain {
    http: reqwest::Client,
    cache: Mutex<Option<Cached>>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RoleCreds {
    access_key_id: String,
    secret_access_key: String,
    token: Option<String>,
    expiration: Option<String>,
}

impl RoleCreds {
    fn cached(self) -> Cached {
        Cached {
            access_key_id: self.access_key_id,
            secret_access_key: self.secret_access_key,
            session_token: self.token,
            expires: self
                .expiration
                .and_then(|e| chrono::DateTime::parse_from_rfc3339(&e).ok())
                .map(|e| e.with_timezone(&chrono::Utc)),
        }
    }
}

impl CredentialChain {
    pub fn new() -> Result<Self> {
        Ok(CredentialChain {
            // Metadata endpoints are link-local; never send them through a proxy.
            http: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(3))
                .build()?,
            cache: Mutex::new(None),
        })
    }

    pub async fn get(&self) -> Result<Credentials> {
        if let (Ok(id), Ok(secret)) = (
            std::env::var("AWS_ACCESS_KEY_ID"),
            std::env::var("AWS_SECRET_ACCESS_KEY"),
        ) {
            return Ok(Credentials {
                access_key_id: id,
                secret_access_key: secret,
                session_token: std::env::var("AWS_SESSION_TOKEN").ok(),
            });
        }
        let mut cache = self.cache.lock().await;
        if let Some(c) = cache.as_ref().filter(|c| c.fresh()) {
            return Ok(c.creds());
        }
        let fetched = if let Ok(rel) = std::env::var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI") {
            self.container(&format!("http://169.254.170.2{rel}"))
                .await?
        } else if let Ok(full) = std::env::var("AWS_CONTAINER_CREDENTIALS_FULL_URI") {
            self.container(&full).await?
        } else {
            self.instance().await?
        };
        let creds = fetched.creds();
        *cache = Some(fetched);
        Ok(creds)
    }

    async fn container(&self, url: &str) -> Result<Cached> {
        let mut req = self.http.get(url);
        if let Ok(token) = std::env::var("AWS_CONTAINER_AUTHORIZATION_TOKEN") {
            req = req.header("Authorization", token);
        }
        let r: RoleCreds = req
            .send()
            .await
            .context("reaching the container credentials endpoint")?
            .error_for_status()?
            .json()
            .await?;
        Ok(r.cached())
    }

    async fn instance(&self) -> Result<Cached> {
        const IMDS: &str = "http://169.254.169.254";
        let token = self
            .http
            .put(format!("{IMDS}/latest/api/token"))
            .header("X-aws-ec2-metadata-token-ttl-seconds", "21600")
            .send()
            .await
            .context("no AWS credentials: no environment variables, no container endpoint, no instance metadata")?
            .error_for_status()?
            .text()
            .await?;
        let base = format!("{IMDS}/latest/meta-data/iam/security-credentials/");
        let roles = self
            .http
            .get(&base)
            .header("X-aws-ec2-metadata-token", &token)
            .send()
            .await?
            .error_for_status()
            .context("the instance has no role")?
            .text()
            .await?;
        let Some(role) = roles.lines().next().filter(|r| !r.is_empty()) else {
            bail!("the instance has no role");
        };
        let r: RoleCreds = self
            .http
            .get(format!("{base}{role}"))
            .header("X-aws-ec2-metadata-token", &token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(r.cached())
    }
}
