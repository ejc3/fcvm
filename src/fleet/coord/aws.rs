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

/// One signed AWS request. `base` is `https://<host>` (or a test server's `http://` URL);
/// the `host` header is taken from it. S3 wants its payload hash in a header
/// (`x-amz-content-sha256`), which is added for every service, harmlessly.
#[allow(clippy::too_many_arguments)]
pub async fn signed(
    chain: &CredentialChain,
    http: &reqwest::Client,
    base: &str,
    method: reqwest::Method,
    path: &str,
    query: &str,
    region: &str,
    service: &str,
    extra: &[(&str, String)],
    body: Vec<u8>,
) -> Result<(reqwest::StatusCode, Vec<u8>)> {
    use sha2::{Digest, Sha256};
    let host = base
        .split_once("://")
        .map(|(_, h)| h)
        .unwrap_or(base)
        .trim_end_matches('/')
        .to_string();
    let creds = chain.get().await?;
    let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let mut headers = vec![
        ("host".to_string(), host),
        ("x-amz-date".to_string(), amz_date.clone()),
        (
            "x-amz-content-sha256".to_string(),
            hex::encode(Sha256::digest(&body)),
        ),
    ];
    for (k, v) in extra {
        headers.push((k.to_ascii_lowercase(), v.clone()));
    }
    if let Some(t) = &creds.session_token {
        headers.push(("x-amz-security-token".to_string(), t.clone()));
    }
    let auth = super::sigv4::authorization(
        &creds,
        method.as_str(),
        path,
        query,
        &headers,
        &body,
        region,
        service,
        &amz_date,
    );
    let url = if query.is_empty() {
        format!("{}{path}", base.trim_end_matches('/'))
    } else {
        format!("{}{path}?{query}", base.trim_end_matches('/'))
    };
    let mut req = http.request(method, url).header("authorization", auth);
    for (k, v) in &headers {
        if k != "host" {
            req = req.header(k, v);
        }
    }
    let r = req
        .body(body)
        .send()
        .await
        .with_context(|| format!("calling {service}"))?;
    let status = r.status();
    let bytes = r.bytes().await?.to_vec();
    Ok((status, bytes))
}

/// A value from the instance metadata service (IMDSv2), e.g. `instance-id`.
pub async fn imds(path: &str) -> Result<String> {
    const IMDS: &str = "http://169.254.169.254";
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()?;
    let token = http
        .put(format!("{IMDS}/latest/api/token"))
        .header("X-aws-ec2-metadata-token-ttl-seconds", "300")
        .send()
        .await
        .context("reaching the instance metadata service")?
        .error_for_status()?
        .text()
        .await?;
    Ok(http
        .get(format!("{IMDS}/latest/meta-data/{path}"))
        .header("X-aws-ec2-metadata-token", &token)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?
        .trim()
        .to_string())
}

/// The top-level `<item>` elements of every `<set>` list in an AWS XML answer (EC2's
/// `instancesSet`, `spotInstanceRequestSet`, ...), each with everything nested in it. EC2 does
/// not promise an order for an item's elements, so an item, not one of its fields, bounds a
/// record. Nested lists (tag sets, interfaces) are inside their item.
pub fn set_items<'a>(xml: &'a str, set: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{set}>"), format!("</{set}>"));
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let body = &rest[i + open.len()..];
        let end = body.find(&close).unwrap_or(body.len());
        let list = &body[..end];
        let (mut depth, mut start, mut at) = (0usize, 0usize, 0usize);
        while at < list.len() {
            let tail = &list[at..];
            if tail.starts_with("<item>") {
                if depth == 0 {
                    start = at + "<item>".len();
                }
                depth += 1;
                at += "<item>".len();
            } else if tail.starts_with("</item>") {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    out.push(&list[start..at]);
                }
                at += "</item>".len();
            } else {
                at += tail.chars().next().map_or(1, char::len_utf8);
            }
        }
        rest = &body[end..];
    }
    out
}

/// The text of the first `<name>` element in an AWS XML answer.
pub fn xml_tag(xml: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].trim().to_string())
}
