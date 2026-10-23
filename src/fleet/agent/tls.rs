//! The fleet's wildcard certificate for the renderers on this host: read from its secret
//! with the instance role, written as `fullchain.pem` and `key.pem` (mode 0600, replaced
//! atomically) into a directory each VM gets mapped read-only, and read again every hour so
//! a renewal reaches the renderers. The key is never logged.

use crate::fleet::coord::secrets::Secrets;
use anyhow::{Context, Result};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Where the files appear inside a VM, and the environment telling the renderer.
pub const GUEST_DIR: &str = "/run/fleet-tls";

/// The `fcvm podman run` arguments that give a VM the certificate directory.
pub fn vm_args(dir: &Path) -> Vec<String> {
    vec![
        "--map".into(),
        format!("{}:{GUEST_DIR}", dir.display()),
        "--env".into(),
        format!("TLS_CERT_FILE={GUEST_DIR}/fullchain.pem"),
        "--env".into(),
        format!("TLS_KEY_FILE={GUEST_DIR}/key.pem"),
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

/// Fetch the secret and write the files when they differ from what is there. Returns
/// whether anything changed.
pub async fn refresh(secrets: &Secrets, secret: &str, dir: &Path) -> Result<bool> {
    let v: serde_json::Value = serde_json::from_str(&secrets.get(secret).await?)
        .with_context(|| format!("secret {secret} is not JSON"))?;
    let field = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .with_context(|| format!("secret {secret} has no {k}"))
    };
    let (cert, key) = (field("cert")?, field("key")?);
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let mut changed = false;
    for (name, body) in [("fullchain.pem", &cert), ("key.pem", &key)] {
        let path = dir.join(name);
        if std::fs::read(&path).ok().as_deref() != Some(body.as_bytes()) {
            write_private(&path, body.as_bytes())?;
            changed = true;
        }
    }
    Ok(changed)
}

/// Keep the files current, hourly.
pub async fn run(secrets: Secrets, secret: String, dir: std::path::PathBuf) {
    loop {
        match refresh(&secrets, &secret, &dir).await {
            Ok(true) => tracing::info!(dir = %dir.display(), "renderer certificate updated"),
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "renderer certificate not refreshed")
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    }
}
