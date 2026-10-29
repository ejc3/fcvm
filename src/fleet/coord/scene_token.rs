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

/// The secret's token, after adding one when it has none. Two coordinators that add one at
/// once agree: each writes with a request token derived from the version it read, so the
/// service stores only the first write of that version and refuses the other, which then
/// reads the stored token.
pub async fn ensure(secrets: &Secrets, secret: &str) -> Result<String> {
    ensure_with(secrets, secret, None).await
}

/// As [`ensure`], adding `token` (a new one when None) when the secret has none.
async fn ensure_with(secrets: &Secrets, secret: &str, token: Option<&str>) -> Result<String> {
    let (text, version) = secrets
        .get_version(secret)
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
    obj.insert(
        "scene_token".into(),
        token.map(str::to_string).unwrap_or_else(generate).into(),
    );
    let request = request_token(version.as_deref().unwrap_or(""));
    let put = secrets
        .put_with_token(secret, &doc.to_string(), &request)
        .await;
    // Refused because another writer stored this version's successor first: its token stands.
    let stored = from_secret(&secrets.get(secret).await?);
    match (put, stored) {
        (_, Some(t)) => {
            tracing::info!(secret, "the certificate secret has a scene token");
            Ok(t)
        }
        (Err(e), None) => Err(e).with_context(|| format!("storing a scene token in {secret}")),
        (Ok(()), None) => {
            anyhow::bail!("secret {secret} has no scene token after it was stored")
        }
    }
}

/// The `ClientRequestToken` of the write that adds a token to `version`: the same for every
/// writer that read that version.
fn request_token(version: &str) -> String {
    let d = ring::digest::digest(
        &ring::digest::SHA256,
        format!("fcvm-fleet scene token for {version}").as_bytes(),
    );
    d.as_ref()[..24]
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

/// Keep the token in the secret, hourly: a version without one (a certificate stored by a
/// tool that dropped it) gets this coordinator's token back, so the renderers that read the
/// secret keep taking its scenes. A different token in the secret is not followed: running
/// tiles read theirs once at boot, so switching would cut them off. Rotating the token means
/// restarting the coordinator and relaunching the tiles and hosts (fleet/PROTOCOL.md,
/// "Access").
pub async fn keep(secrets: Secrets, secret: String, token: SceneToken) {
    let Some(held) = token.get() else { return };
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        match ensure_with(&secrets, &secret, Some(&held)).await {
            Ok(t) if t == held => {}
            Ok(_) => tracing::warn!(
                secret,
                "the certificate secret holds another scene token; restart the coordinator and relaunch the tiles to rotate it"
            ),
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "the scene token was not checked"),
        }
    }
}
