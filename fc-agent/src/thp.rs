//! The guest's transparent huge page mode (`--guest-thp`).
//!
//! A clone's memory is a copy-on-write view of its snapshot, so every guest page
//! it writes becomes a private host page. With THP `always` a fault anywhere in
//! a large enough anonymous mapping allocates and zeroes a whole 2 MiB page, and
//! the clone pays for all 512 pages it wrote. The mode is applied at boot from
//! the boot plan and again at every restore from the restore document, so it is
//! not part of the snapshot key: a restore sets its own.
//!
//! `default` names the mode the guest kernel booted with, recorded here before
//! anything changes it. A run that gives no mode and restores a snapshot made by
//! one that did gets the kernel's own mode back, as its cold boot would have.

use std::path::Path;
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};

const ENABLED: &str = "/sys/kernel/mm/transparent_hugepage/enabled";

/// The modes the kernel accepts in `enabled`.
const MODES: [&str; 3] = ["always", "madvise", "never"];

static BOOT_DEFAULT: OnceLock<String> = OnceLock::new();

/// The selected mode in the contents of `enabled`: the bracketed word of
/// `always [madvise] never`.
fn selected(contents: &str) -> Option<&str> {
    let start = contents.find('[')? + 1;
    let end = start + contents[start..].find(']')?;
    Some(&contents[start..end])
}

fn read_selected(path: &Path) -> Result<String> {
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    selected(&contents)
        .map(str::to_string)
        .with_context(|| format!("{} has no selected mode: {contents:?}", path.display()))
}

/// Record the kernel's boot-time mode. Called once, before any mode is applied.
pub fn record_boot_default() {
    match read_selected(Path::new(ENABLED)) {
        Ok(mode) => {
            let _ = BOOT_DEFAULT.set(mode);
        }
        Err(error) => eprintln!("[fc-agent] transparent huge pages: {error:#}"),
    }
}

/// Apply `mode` (`always`, `madvise`, `never` or `default`).
pub fn apply(mode: &str) -> Result<()> {
    apply_at(
        Path::new(ENABLED),
        mode,
        BOOT_DEFAULT.get().map(String::as_str),
    )
}

fn apply_at(path: &Path, mode: &str, boot_default: Option<&str>) -> Result<()> {
    let wanted = match mode {
        "default" => boot_default.context("the boot-time transparent huge page mode is unknown")?,
        mode if MODES.contains(&mode) => mode,
        other => bail!("unknown transparent huge page mode {other:?}"),
    };
    std::fs::write(path, wanted)
        .with_context(|| format!("writing {wanted} to {}", path.display()))?;
    let now = read_selected(path)?;
    if now != wanted {
        bail!("{} selects {now} after writing {wanted}", path.display());
    }
    eprintln!("[fc-agent] transparent huge pages: {wanted}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_selected_mode_is_the_bracketed_word() {
        assert_eq!(selected("always [madvise] never\n"), Some("madvise"));
        assert_eq!(selected("[always] madvise never"), Some("always"));
        assert_eq!(selected("always madvise never"), None);
    }

    #[test]
    fn default_needs_the_recorded_boot_mode_and_unknown_modes_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("enabled");
        std::fs::write(&path, "[always] madvise never").unwrap();
        assert!(
            apply_at(&path, "default", None).is_err(),
            "no recorded boot mode"
        );
        assert!(apply_at(&path, "sometimes", Some("always")).is_err());
        // Neither refusal wrote anything.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[always] madvise never"
        );
    }
}
