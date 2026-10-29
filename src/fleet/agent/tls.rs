//! The fleet's wildcard certificate for the renderers on this host, in the renderer's TLS
//! interface (fleet/PROTOCOL.md, "TLS", round 6 of the renderer): read from its secret with
//! the instance role, written as `tls.json`, a JSON object `{"cert", "key"}` (the leaf and its
//! intermediates, PEM; the leaf's key, PEM), mode 0600 and replaced atomically, into a
//! directory each VM gets mapped read-only at `/run/fleet-tls`, with `TLS_CERT_FILE` naming
//! it and `TLS_PORT` 443. The secret is read again every hour. A renewal is a new file
//! renamed into place; the renderer checks the file every 2 s and loads a new version for
//! the connections that follow, so a renewal reaches every renderer without a restart. The
//! key is never logged.
//!
//! The file also carries the secret's `scene_token` (fleet/PROTOCOL.md, "Access"), which the
//! renderer requires on `POST /scene` and `/budget`, and which the agent sends with its own.

use crate::fleet::coord::scene_token::SceneToken;
use crate::fleet::coord::secrets::Secrets;
use anyhow::{Context, Result};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Where the agent keeps the file on the host by default (`--tls-dir`). Mapped into every
/// VM, so it is part of the VMs' snapshot key, which the AMI's warm-up
/// (fleet/ami/host/warm-root.sh) must match.
pub const HOST_DIR: &str = "/run/fcvm-fleet/tls";

/// Where the file appears inside a VM, its name, and the renderer's TLS port.
pub const GUEST_DIR: &str = "/run/fleet-tls";
pub const FILE: &str = "tls.json";
pub const PORT: u16 = 443;

/// The `fcvm podman run` arguments that give a VM the certificate directory.
pub fn vm_args(dir: &Path) -> Vec<String> {
    vec![
        "--map".into(),
        format!("{}:{GUEST_DIR}:ro", dir.display()),
        "--env".into(),
        format!("TLS_CERT_FILE={GUEST_DIR}/{FILE}"),
        "--env".into(),
        format!("TLS_PORT={PORT}"),
    ]
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("a file with no directory")?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)?;
    Ok(())
}

/// Fetch the secret and write the file when it differs from what is there. Returns
/// whether it changed. The secret's scene token goes into the file and into `token`.
pub async fn refresh(
    secrets: &Secrets,
    secret: &str,
    dir: &Path,
    token: &SceneToken,
) -> Result<bool> {
    let v: serde_json::Value = serde_json::from_str(&secrets.get(secret).await?)
        .with_context(|| format!("secret {secret} is not JSON"))?;
    let field = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .with_context(|| format!("secret {secret} has no {k}"))
    };
    let mut doc = serde_json::json!({"cert": field("cert")?, "key": field("key")?});
    let scene = v
        .get("scene_token")
        .and_then(|x| x.as_str())
        .filter(|t| !t.is_empty());
    if let Some(t) = scene {
        doc["scene_token"] = t.into();
    }
    let body = serde_json::to_vec(&doc)?;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let path = dir.join(FILE);
    let changed = std::fs::read(&path).ok().as_deref() != Some(body.as_slice());
    if changed {
        write_private(&path, &body)?;
    }
    // Only once the file holds it, so the agent never sends a token its renderers lack.
    if scene.is_some() {
        token.set(scene.map(str::to_string));
    }
    Ok(changed)
}

/// Keep the files current, hourly.
pub async fn run(secrets: Secrets, secret: String, dir: std::path::PathBuf, token: SceneToken) {
    loop {
        match refresh(&secrets, &secret, &dir, &token).await {
            Ok(true) => tracing::info!(dir = %dir.display(), "renderer certificate updated"),
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "renderer certificate not refreshed")
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    }
}
