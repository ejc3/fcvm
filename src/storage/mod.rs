pub mod compact;
pub mod disk;
pub mod disk_dir;
pub mod extents;
pub mod hugepage_spares;
pub mod inherit;
pub mod inode_flags;
pub mod pmem;
pub mod pmem_store;
pub mod snapshot;
pub mod snapshot_archive;
pub mod volume;

pub use disk::{DiskConfig, DiskManager};
pub use snapshot::{
    validate_new_snapshot_name, validate_snapshot_name, SnapshotConfig, SnapshotExtraDisk,
    SnapshotGeneration, SnapshotKind, SnapshotManager, SnapshotMetadata, SnapshotType,
    SnapshotVolumeConfig,
};
pub use volume::{VolumeManager, VolumeMount};

#[cfg(test)]
mod helper_spawn_tests {
    use std::path::Path;

    /// `source` without its top-level `#[cfg(test)] mod` blocks, each of which ends at the
    /// first `}` in column 0 after it.
    fn without_test_modules(source: &str) -> String {
        let mut out = String::new();
        let mut rest = source;
        while let Some(at) = rest.find("#[cfg(test)]\nmod ") {
            out.push_str(&rest[..at]);
            let end = rest[at..]
                .find("\n}\n")
                .map(|end| at + end + 3)
                .unwrap_or(rest.len());
            rest = &rest[end..];
        }
        out.push_str(rest);
        out
    }

    /// The body of `function` in `source`: from its signature to the first `}` in column 0.
    fn body_of<'a>(source: &'a str, function: &str) -> &'a str {
        let start = source
            .find(function)
            .unwrap_or_else(|| panic!("no {function}"));
        let end = source[start..].find("\n}\n").expect("an end") + start;
        &source[start..end]
    }

    /// Every program the storage and snapshot paths run is a helper that the kernel kills
    /// when fcvm exits (`crate::utils::helper_command`). A create's `btrfs subvolume create`
    /// or `btrfs subvolume snapshot` spawned bare outlived a SIGKILLed create: stopped before
    /// it made the staging directory, it made it after the next launch's reclaim pass had
    /// removed the record of it, and nothing then removed the directory (#1918). The same
    /// holds for the clone's subvolume snapshot and reflink, and for `truncate`, which
    /// creates the disk it is asked to extend when the disk is gone.
    #[test]
    fn storage_and_snapshot_helpers_die_with_fcvm() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/storage");
        let mut checked = Vec::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "rs") {
                checked.push((path.clone(), std::fs::read_to_string(&path).unwrap()));
            }
        }
        assert!(
            checked.len() >= 13,
            "found only {} files under {}",
            checked.len(),
            dir.display()
        );
        for file in ["snapshots.rs", "snapshot_export.rs", "snapshot_import.rs"] {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src/commands")
                .join(file);
            checked.push((path.clone(), std::fs::read_to_string(&path).unwrap()));
        }
        // `snapshot.rs` holds one other spawn, `spawn_refill`, which starts a detached
        // `fcvm gpu spares fill` (setsid) that is meant to outlive the restore.
        let snapshot = include_str!("../commands/snapshot.rs");
        let refill = body_of(snapshot, "fn spawn_refill(");
        assert!(refill.contains("libc::setsid()"), "{refill}");
        checked.push((
            "src/commands/snapshot.rs".into(),
            snapshot.replacen(refill, "", 1),
        ));
        let common = include_str!("../commands/common.rs");
        checked.push((
            "src/commands/common.rs reflink_copy".into(),
            body_of(common, "pub(crate) async fn reflink_copy(").to_string(),
        ));
        let mut bare = Vec::new();
        for (path, source) in &checked {
            let code = without_test_modules(source);
            for line in code.lines() {
                if line.contains("Command::new(") && !line.trim_start().starts_with("//") {
                    bare.push(format!("{}: {}", path.display(), line.trim()));
                }
            }
        }
        assert!(
            bare.is_empty(),
            "helpers spawned without crate::utils::helper_command, which outlive a killed \
             fcvm:\n{}",
            bare.join("\n")
        );
        let disk = without_test_modules(include_str!("disk.rs"));
        for function in [
            "pub async fn create_dir_or_subvolume(",
            "pub async fn stage_from_parent_subvolume(",
            "pub async fn snapshot_disk_subvolume(",
            "async fn snapshot_base_subvolume(",
        ] {
            assert!(
                body_of(&disk, function).contains("crate::utils::helper_command(\"btrfs\")"),
                "{function} spawns no helper_command"
            );
        }
    }
}
