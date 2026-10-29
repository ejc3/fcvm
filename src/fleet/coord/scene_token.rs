//! The scene token (fleet/PROTOCOL.md, "Access"): the secret a fleet renderer checks on
//! `POST /scene` and `POST /budget`. It is the `scene_token` field of the wildcard
//! certificate's secret, so it reaches every renderer the way the certificate does: the host
//! agent copies it into each VM's `tls.json`, a tile instance writes it beside nginx's
//! certificate. The coordinator adds one when the secret has none, keeps it through every
//! renewal, and sends it as a bearer token, as the host agents do.

use super::secrets::Secrets;
use anyhow::{Context, Result};
use rand::RngCore;
use std::sync::{Arc, RwLock};

/// The token a client holds, shared between the task that keeps it current and the ones that
/// post with it. Empty: renderers that need none (a local run).
#[derive(Clone, Default)]
pub struct SceneToken(Arc<RwLock<Option<String>>>);

/// Shows whether a token is held, never the token.
impl std::fmt::Debug for SceneToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let held = if self.get().is_some() { "held" } else { "none" };
        f.debug_tuple("SceneToken").field(&held).finish()
    }
}

impl SceneToken {
    pub fn new(token: Option<String>) -> Self {
        SceneToken(Arc::new(RwLock::new(token)))
    }

    pub fn set(&self, token: Option<String>) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = token;
    }

    pub fn get(&self) -> Option<String> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// `request` with `Authorization: Bearer <token>` when there is one.
    pub fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.get() {
            Some(t) => request.bearer_auth(t),
            None => request,
        }
    }
}

/// A new token: 32 random bytes, hex.
pub fn generate() -> String {
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The token in a secret's JSON text, if it has one.
pub fn from_secret(text: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()?
        .get("scene_token")?
        .as_str()
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// The secret's token, after adding one when it has none. The secret is read again after the
/// write and its token returned, so two coordinators adding one at once agree on the version
/// that was stored last.
pub async fn ensure(secrets: &Secrets, secret: &str) -> Result<String> {
    let text = secrets
        .get(secret)
        .await
        .with_context(|| format!("reading {secret}"))?;
    if let Some(t) = from_secret(&text) {
        return Ok(t);
    }
    let mut doc: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("secret {secret} is not JSON"))?;
    let obj = doc
        .as_object_mut()
        .with_context(|| format!("secret {secret} is not a JSON object"))?;
    obj.insert("scene_token".into(), generate().into());
    secrets
        .put(secret, &doc.to_string())
        .await
        .with_context(|| format!("storing a scene token in {secret}"))?;
    tracing::info!(secret, "added a scene token to the certificate secret");
    from_secret(&secrets.get(secret).await?)
        .with_context(|| format!("secret {secret} has no scene token after it was stored"))
}

/// Follow the secret's token hourly, so a token rotated by hand reaches this client without a
/// restart. A secret that cannot be read keeps the token held.
pub async fn follow(secrets: Secrets, secret: String, token: SceneToken) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        match secrets.get(&secret).await {
            Ok(text) => {
                if let Some(t) = from_secret(&text) {
                    token.set(Some(t));
                }
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "the scene token was not refreshed")
            }
        }
    }
}
