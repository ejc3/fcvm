//! Inode-remapping filesystem wrapper for portable snapshots.
//!
//! `RemapFs` wraps any `FilesystemHandler` and translates between stable
//! path-hash-based inodes (deterministic, portable across machines) and the
//! inner handler's host-specific inodes.
//!
//! When `--portable-volumes` is set, the volume server wraps `PassthroughFs`
//! in `RemapFs` so that snapshot/clone operations produce consistent inode
//! numbers regardless of which host they run on.

use dashmap::DashMap;
use tracing::{debug, error, info};

use crate::protocol::{VolumeRequest, VolumeResponse};

use super::handler::FilesystemHandler;

/// Filesystem wrapper that remaps inodes for portable snapshots.
///
/// Translates between stable (path-hash) inodes visible to FUSE clients
/// and inner (host-specific) inodes used by the underlying filesystem.
pub struct RemapFs<T: FilesystemHandler> {
    inner: T,
    /// inner_ino → stable_ino (set on first encounter, never changes)
    inner_to_stable: DashMap<u64, u64>,
    /// stable_ino → inner_ino (reverse mapping for request remapping)
    stable_to_inner: DashMap<u64, u64>,
    /// stable_ino → relative path from mount root (for serialization)
    paths: DashMap<u64, String>,
}

impl<T: FilesystemHandler> RemapFs<T> {
    /// Create a new RemapFs wrapping the given handler.
    ///
    /// Initializes the root inode mapping (stable:1 ↔ inner:1, path:"").
    pub fn new(inner: T) -> Self {
        let inner_to_stable = DashMap::new();
        let stable_to_inner = DashMap::new();
        let paths = DashMap::new();

        // Root inode is always 1 in FUSE
        inner_to_stable.insert(1, 1);
        stable_to_inner.insert(1, 1);
        paths.insert(1, String::new());

        Self {
            inner,
            inner_to_stable,
            stable_to_inner,
            paths,
        }
    }

    /// FNV-1a hash of a path string, used to compute stable inodes.
    ///
    /// Returns a value >= 2 (0 is invalid, 1 is reserved for root).
    pub fn path_hash(path: &str) -> u64 {
        let mut hash: u64 = 0xcbf29ce484222325; // FNV-1a offset basis
        for byte in path.as_bytes() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x100000001b3); // FNV-1a prime
        }
        // Avoid reserved values: 0 (invalid) and 1 (root)
        if hash < 2 {
            hash + 2
        } else {
            hash
        }
    }

    /// Build a relative path from parent path and child name.
    fn format_path(parent_path: &str, name: &str) -> String {
        if parent_path.is_empty() {
            name.to_string()
        } else {
            format!("{}/{}", parent_path, name)
        }
    }

    /// Look up the inner inode for a stable inode.
    fn to_inner(&self, stable: u64) -> Option<u64> {
        self.stable_to_inner.get(&stable).map(|r| *r)
    }

    /// Look up the stable inode for an inner inode.
    fn to_stable(&self, inner: u64) -> Option<u64> {
        self.inner_to_stable.get(&inner).map(|r| *r)
    }

    /// Register a new inode mapping from an entry-returning operation.
    ///
    /// If `inner_ino` is already mapped (hard link case), returns the
    /// existing stable_ino without modifying paths.
    ///
    /// Returns `Err(())` if a hash collision is detected and no alternative
    /// stable_ino can be found.
    fn register_entry(&self, parent_stable: u64, name: &[u8], inner_ino: u64) -> Result<u64, ()> {
        // Atomic check-and-insert via DashMap::entry() — holds a shard write lock
        // so concurrent threads discovering the same inner_ino don't race.
        use dashmap::mapref::entry::Entry;
        match self.inner_to_stable.entry(inner_ino) {
            Entry::Occupied(e) => {
                // Hard link or concurrent duplicate — return existing mapping
                Ok(*e.get())
            }
            Entry::Vacant(e) => {
                // Compute path and hash while holding the shard lock
                let name_str = String::from_utf8_lossy(name);
                let parent_path = self
                    .paths
                    .get(&parent_stable)
                    .map(|p| p.value().clone())
                    .unwrap_or_default();
                let path = Self::format_path(&parent_path, &name_str);
                let mut stable_ino = Self::path_hash(&path);

                // Handle conflicts (true hash collision or rename-then-recreate)
                if self.stable_to_inner.contains_key(&stable_ino) {
                    debug!(
                        stable_ino,
                        path = %path,
                        "stable_ino taken, finding alternative"
                    );
                    let mut found = false;
                    for i in 1u64..10000 {
                        let alt = stable_ino.wrapping_add(i);
                        if alt >= 2 && !self.stable_to_inner.contains_key(&alt) {
                            stable_ino = alt;
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        error!(path = %path, "failed to find free stable_ino");
                        return Err(());
                    }
                }

                // Register bidirectional mapping
                e.insert(stable_ino);
                self.stable_to_inner.insert(stable_ino, inner_ino);
                self.paths.insert(stable_ino, path);

                Ok(stable_ino)
            }
        }
    }

    /// Remap inode fields in a request from stable to inner.
    ///
    /// Returns `None` if a required inode mapping is missing.
    fn remap_request_inodes(&self, req: &mut VolumeRequest) -> Option<()> {
        match req {
            // Single `ino` field
            VolumeRequest::Getattr { ino }
            | VolumeRequest::Readlink { ino }
            | VolumeRequest::Statfs { ino } => {
                *ino = self.to_inner(*ino)?;
            }

            VolumeRequest::Setattr { ino, .. }
            | VolumeRequest::Open { ino, .. }
            | VolumeRequest::Read { ino, .. }
            | VolumeRequest::Write { ino, .. }
            | VolumeRequest::Access { ino, .. }
            | VolumeRequest::Opendir { ino, .. }
            | VolumeRequest::Setxattr { ino, .. }
            | VolumeRequest::Getxattr { ino, .. }
            | VolumeRequest::Listxattr { ino, .. }
            | VolumeRequest::Removexattr { ino, .. }
            | VolumeRequest::Readdir { ino, .. }
            | VolumeRequest::Readdirplus { ino, .. } => {
                *ino = self.to_inner(*ino)?;
            }

            VolumeRequest::Release { ino, .. }
            | VolumeRequest::Flush { ino, .. }
            | VolumeRequest::Fsync { ino, .. }
            | VolumeRequest::Releasedir { ino, .. }
            | VolumeRequest::Fsyncdir { ino, .. }
            | VolumeRequest::Fallocate { ino, .. }
            | VolumeRequest::Lseek { ino, .. }
            | VolumeRequest::Getlk { ino, .. }
            | VolumeRequest::Setlk { ino, .. } => {
                *ino = self.to_inner(*ino)?;
            }

            // Single `parent` field
            VolumeRequest::Lookup { parent, .. }
            | VolumeRequest::Mkdir { parent, .. }
            | VolumeRequest::Create { parent, .. }
            | VolumeRequest::Mknod { parent, .. }
            | VolumeRequest::Rmdir { parent, .. }
            | VolumeRequest::Unlink { parent, .. }
            | VolumeRequest::Symlink { parent, .. } => {
                *parent = self.to_inner(*parent)?;
            }

            // `ino` + `newparent`
            VolumeRequest::Link { ino, newparent, .. } => {
                *ino = self.to_inner(*ino)?;
                *newparent = self.to_inner(*newparent)?;
            }

            // `parent` + `newparent`
            VolumeRequest::Rename {
                parent, newparent, ..
            } => {
                *parent = self.to_inner(*parent)?;
                *newparent = self.to_inner(*newparent)?;
            }

            // `ino_in` + `ino_out`
            VolumeRequest::CopyFileRange {
                ino_in, ino_out, ..
            }
            | VolumeRequest::RemapFileRange {
                ino_in, ino_out, ..
            } => {
                *ino_in = self.to_inner(*ino_in)?;
                *ino_out = self.to_inner(*ino_out)?;
            }

            // Forget: best-effort (don't fail if mapping missing)
            VolumeRequest::Forget { ino, .. } => {
                if let Some(inner) = self.to_inner(*ino) {
                    *ino = inner;
                }
            }

            VolumeRequest::BatchForget { inodes } => {
                for (ino, _nlookup) in inodes.iter_mut() {
                    if let Some(inner) = self.to_inner(*ino) {
                        *ino = inner;
                    }
                }
            }
        }
        Some(())
    }

    /// Remap inodes in a response from inner to stable, registering new mappings.
    ///
    /// `original_req` is the request with STABLE inodes (before remapping),
    /// needed to compute paths for newly discovered entries.
    fn remap_response(
        &self,
        original_req: &VolumeRequest,
        response: VolumeResponse,
    ) -> VolumeResponse {
        if response.is_error() {
            return response;
        }

        match original_req {
            // Entry-returning ops: register mapping, remap attr.ino
            VolumeRequest::Lookup { parent, name, .. }
            | VolumeRequest::Mkdir { parent, name, .. }
            | VolumeRequest::Create { parent, name, .. }
            | VolumeRequest::Mknod { parent, name, .. }
            | VolumeRequest::Symlink { parent, name, .. } => {
                self.remap_entry_response(*parent, name, response)
            }

            // Link: use newparent/newname for path, inner_ino already mapped
            VolumeRequest::Link {
                newparent, newname, ..
            } => self.remap_entry_response(*newparent, newname, response),

            // Attr-returning ops: just remap ino
            VolumeRequest::Getattr { .. } | VolumeRequest::Setattr { .. } => {
                self.remap_attr_response(response)
            }

            // Readdir: remap entries, detect stale paths
            VolumeRequest::Readdir { ino, .. } => self.remap_readdir_response(*ino, response),

            // Readdirplus: remap + register entries, detect stale paths
            VolumeRequest::Readdirplus { ino, .. } => {
                self.remap_readdirplus_response(*ino, response)
            }

            // Rename: update paths on success
            VolumeRequest::Rename {
                parent,
                name,
                newparent,
                newname,
                flags,
                ..
            } => {
                self.handle_rename_paths(*parent, name, *newparent, newname, *flags);
                response
            }

            // Everything else: pass through unchanged
            _ => response,
        }
    }

    /// Remap an Entry or Created response, registering the new inode mapping.
    fn remap_entry_response(
        &self,
        parent_stable: u64,
        name: &[u8],
        mut response: VolumeResponse,
    ) -> VolumeResponse {
        match &mut response {
            VolumeResponse::Entry { attr, .. } | VolumeResponse::Created { attr, .. } => {
                let inner_ino = attr.ino;
                match self.register_entry(parent_stable, name, inner_ino) {
                    Ok(stable_ino) => {
                        attr.ino = stable_ino;
                        response
                    }
                    Err(()) => VolumeResponse::io_error(),
                }
            }
            _ => response,
        }
    }

    /// Remap an Attr response's inode from inner to stable.
    fn remap_attr_response(&self, mut response: VolumeResponse) -> VolumeResponse {
        if let VolumeResponse::Attr { attr, .. } = &mut response {
            if let Some(stable) = self.to_stable(attr.ino) {
                attr.ino = stable;
            }
        }
        response
    }

    /// Remap readdir entries from inner to stable inodes.
    ///
    /// Readdir entries are advisory (don't bump nlookup), so we don't
    /// register new mappings. We do detect and fix stale paths.
    fn remap_readdir_response(
        &self,
        dir_stable: u64,
        mut response: VolumeResponse,
    ) -> VolumeResponse {
        if let VolumeResponse::DirEntries { entries } = &mut response {
            let dir_path = self
                .paths
                .get(&dir_stable)
                .map(|p| p.value().clone())
                .unwrap_or_default();

            for entry in entries.iter_mut() {
                if entry.name == b"." {
                    entry.ino = dir_stable;
                    continue;
                }
                if entry.name == b".." {
                    if let Some(stable) = self.to_stable(entry.ino) {
                        entry.ino = stable;
                    }
                    continue;
                }

                let inner_ino = entry.ino;
                let name_str = String::from_utf8_lossy(&entry.name);
                let expected_path = Self::format_path(&dir_path, &name_str);

                if let Some(existing_stable) = self.inner_to_stable.get(&inner_ino) {
                    let stable = *existing_stable;
                    entry.ino = stable;

                    // Path staleness detection
                    if let Some(mut stored_path) = self.paths.get_mut(&stable) {
                        if *stored_path != expected_path {
                            debug!(
                                old_path = %*stored_path,
                                new_path = %expected_path,
                                "readdir: fixing stale path"
                            );
                            *stored_path = expected_path;
                        }
                    }
                } else {
                    // Unknown inner_ino: compute hash on-the-fly (no registration)
                    entry.ino = Self::path_hash(&expected_path);
                }
            }
        }
        response
    }

    /// Remap readdirplus entries, registering mappings (implicit lookup).
    ///
    /// Readdirplus is an implicit lookup for each entry, so we register
    /// new mappings and detect stale paths.
    fn remap_readdirplus_response(
        &self,
        dir_stable: u64,
        mut response: VolumeResponse,
    ) -> VolumeResponse {
        if let VolumeResponse::DirEntriesPlus { entries } = &mut response {
            let dir_path = self
                .paths
                .get(&dir_stable)
                .map(|p| p.value().clone())
                .unwrap_or_default();

            for entry in entries.iter_mut() {
                if entry.name == b"." {
                    entry.ino = dir_stable;
                    entry.attr.ino = dir_stable;
                    continue;
                }
                if entry.name == b".." {
                    if let Some(stable) = self.to_stable(entry.ino) {
                        entry.ino = stable;
                        entry.attr.ino = stable;
                    }
                    continue;
                }

                let inner_ino = entry.attr.ino;
                match self.register_entry(dir_stable, &entry.name, inner_ino) {
                    Ok(stable_ino) => {
                        entry.ino = stable_ino;
                        entry.attr.ino = stable_ino;

                        // Path staleness detection
                        let name_str = String::from_utf8_lossy(&entry.name);
                        let expected_path = Self::format_path(&dir_path, &name_str);
                        if let Some(mut stored_path) = self.paths.get_mut(&stable_ino) {
                            if *stored_path != expected_path {
                                debug!(
                                    old_path = %*stored_path,
                                    new_path = %expected_path,
                                    "readdirplus: fixing stale path"
                                );
                                *stored_path = expected_path;
                            }
                        }
                    }
                    Err(()) => {
                        // Collision - leave entry with inner ino (rare, logged above)
                    }
                }
            }
        }
        response
    }

    /// Update stored paths after a successful rename.
    fn handle_rename_paths(
        &self,
        parent_stable: u64,
        name: &[u8],
        newparent_stable: u64,
        newname: &[u8],
        flags: u32,
    ) {
        let name_str = String::from_utf8_lossy(name);
        let newname_str = String::from_utf8_lossy(newname);
        let parent_path = self
            .paths
            .get(&parent_stable)
            .map(|p| p.value().clone())
            .unwrap_or_default();
        let newparent_path = self
            .paths
            .get(&newparent_stable)
            .map(|p| p.value().clone())
            .unwrap_or_default();
        let old_path = Self::format_path(&parent_path, &name_str);
        let new_path = Self::format_path(&newparent_path, &newname_str);

        // Find stable_ino of the source entry by scanning paths
        let source_stable = self
            .paths
            .iter()
            .find(|e| *e.value() == old_path)
            .map(|e| *e.key());

        let is_exchange = flags & 2 != 0; // RENAME_EXCHANGE

        if is_exchange {
            // RENAME_EXCHANGE: swap both entries' paths
            let dest_stable = self
                .paths
                .iter()
                .find(|e| *e.value() == new_path)
                .map(|e| *e.key());

            if let Some(src) = source_stable {
                self.paths.insert(src, new_path.clone());
            }
            if let Some(dst) = dest_stable {
                self.paths.insert(dst, old_path.clone());
            }
        } else {
            // Regular rename: update source path and all descendants
            if let Some(src) = source_stable {
                self.paths.insert(src, new_path.clone());

                // Update descendant paths (for directory renames)
                let old_prefix = format!("{}/", old_path);
                let new_prefix = format!("{}/", new_path);
                let updates: Vec<(u64, String)> = self
                    .paths
                    .iter()
                    .filter(|e| e.value().starts_with(&old_prefix))
                    .map(|e| (*e.key(), e.value().replacen(&old_prefix, &new_prefix, 1)))
                    .collect();
                for (ino, path) in updates {
                    self.paths.insert(ino, path);
                }
            }
        }
    }

    /// Serialize the inode mapping table as JSON.
    ///
    /// Returns a JSON object mapping stable_ino (as string key) to path.
    /// Used for portable snapshot restore.
    pub fn serialize_table(&self) -> String {
        let map: std::collections::BTreeMap<u64, String> = self
            .paths
            .iter()
            .map(|e| (*e.key(), e.value().clone()))
            .collect();
        serde_json::to_string(&map).unwrap_or_default()
    }

    /// Restore a RemapFs from a serialized inode table.
    ///
    /// Walks each path in the table through the inner handler's lookup
    /// to rebuild the stable_ino ↔ inner_ino mappings.
    ///
    /// Paths that no longer exist on the host are skipped (logged as warnings).
    pub fn restore_from_table(inner: T, json: &str) -> Self {
        let table: std::collections::BTreeMap<u64, String> =
            serde_json::from_str(json).unwrap_or_default();

        let inner_to_stable = DashMap::new();
        let stable_to_inner = DashMap::new();
        let paths = DashMap::new();

        // Root is always mapped
        inner_to_stable.insert(1, 1);
        stable_to_inner.insert(1, 1);
        paths.insert(1, String::new());

        // Walk each path to discover inner inodes
        for (stable_ino, path) in &table {
            if path.is_empty() {
                continue; // Root already handled
            }

            // Walk each component through lookup
            let components: Vec<&str> = path.split('/').collect();
            let mut current_inner = 1u64; // Start from root
            let mut resolved = true;

            for component in &components {
                let req = VolumeRequest::Lookup {
                    parent: current_inner,
                    name: component.as_bytes().to_vec(),
                    uid: 0,
                    gid: 0,
                    pid: 0,
                };
                let resp = inner.handle_request(&req);
                match resp.attr() {
                    Some(attr) => {
                        current_inner = attr.ino;
                    }
                    None => {
                        debug!(
                            path = %path,
                            component = %component,
                            "restore: path no longer exists, skipping"
                        );
                        resolved = false;
                        break;
                    }
                }
            }

            if resolved {
                inner_to_stable.insert(current_inner, *stable_ino);
                stable_to_inner.insert(*stable_ino, current_inner);
                paths.insert(*stable_ino, path.clone());
            }
        }

        info!(
            restored = paths.len(),
            total = table.len(),
            "RemapFs restored from table"
        );

        Self {
            inner,
            inner_to_stable,
            stable_to_inner,
            paths,
        }
    }

    /// Get a reference to the inner handler.
    pub fn inner(&self) -> &T {
        &self.inner
    }
}

impl<T: FilesystemHandler> FilesystemHandler for RemapFs<T> {
    fn handle_request_with_groups(
        &self,
        request: &VolumeRequest,
        supplementary_groups: &[u32],
    ) -> VolumeResponse {
        // Clone and remap request inodes (stable → inner)
        let mut remapped = request.clone();
        if self.remap_request_inodes(&mut remapped).is_none() {
            error!(op = request.op_name(), "unknown inode in request");
            return VolumeResponse::io_error();
        }

        // Delegate to inner handler
        let response = self
            .inner
            .handle_request_with_groups(&remapped, supplementary_groups);

        // Remap response inodes (inner → stable) and register new mappings
        // Use ORIGINAL request (stable inodes) for path computation
        self.remap_response(request, response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{DirEntry, DirEntryPlus, FileAttr};
    use std::sync::Mutex;

    /// Mock handler that records requests and returns configurable responses.
    struct MockFs {
        next_response: Mutex<Option<VolumeResponse>>,
    }

    impl MockFs {
        fn new() -> Self {
            Self {
                next_response: Mutex::new(None),
            }
        }

        fn set_response(&self, resp: VolumeResponse) {
            *self.next_response.lock().unwrap() = Some(resp);
        }
    }

    impl FilesystemHandler for MockFs {
        fn handle_request_with_groups(
            &self,
            _request: &VolumeRequest,
            _groups: &[u32],
        ) -> VolumeResponse {
            self.next_response
                .lock()
                .unwrap()
                .take()
                .unwrap_or(VolumeResponse::Ok)
        }
    }

    fn make_attr(ino: u64) -> FileAttr {
        FileAttr {
            ino,
            size: 100,
            blocks: 1,
            atime_secs: 0,
            atime_nsecs: 0,
            mtime_secs: 0,
            mtime_nsecs: 0,
            ctime_secs: 0,
            ctime_nsecs: 0,
            mode: libc::S_IFREG | 0o644,
            nlink: 1,
            uid: 1000,
            gid: 1000,
            rdev: 0,
            blksize: 4096,
        }
    }

    #[test]
    fn test_path_hash_deterministic() {
        assert_eq!(
            RemapFs::<MockFs>::path_hash("foo"),
            RemapFs::<MockFs>::path_hash("foo")
        );
        assert_ne!(
            RemapFs::<MockFs>::path_hash("foo"),
            RemapFs::<MockFs>::path_hash("bar")
        );
    }

    #[test]
    fn test_path_hash_avoids_reserved() {
        // Empty string and all inputs should return >= 2
        let h = RemapFs::<MockFs>::path_hash("");
        assert!(h >= 2);
    }

    #[test]
    fn test_basic_lookup_remapping() {
        let mock = MockFs::new();
        let remap = RemapFs::new(mock);

        // Inner handler returns entry with inner_ino=500
        remap.inner.set_response(VolumeResponse::Entry {
            attr: make_attr(500),
            generation: 0,
            ttl_secs: 1,
        });

        let req = VolumeRequest::Lookup {
            parent: 1, // root (stable)
            name: b"hello.txt".to_vec(),
            uid: 1000,
            gid: 1000,
            pid: 1,
        };

        let resp = remap.handle_request_with_groups(&req, &[]);

        // Response should have stable inode (hash of "hello.txt"), not 500
        match resp {
            VolumeResponse::Entry { attr, .. } => {
                assert_ne!(attr.ino, 500, "should not expose inner inode");
                assert_eq!(attr.ino, RemapFs::<MockFs>::path_hash("hello.txt"));
            }
            other => panic!("unexpected response: {:?}", other),
        }
    }

    #[test]
    fn test_consistent_inode_across_lookups() {
        let mock = MockFs::new();
        let remap = RemapFs::new(mock);

        // First lookup
        remap.inner.set_response(VolumeResponse::Entry {
            attr: make_attr(500),
            generation: 0,
            ttl_secs: 1,
        });
        let req = VolumeRequest::Lookup {
            parent: 1,
            name: b"file.txt".to_vec(),
            uid: 0,
            gid: 0,
            pid: 0,
        };
        let resp1 = remap.handle_request_with_groups(&req, &[]);
        let ino1 = resp1.attr().unwrap().ino;

        // Second lookup (same inner_ino)
        remap.inner.set_response(VolumeResponse::Entry {
            attr: make_attr(500),
            generation: 0,
            ttl_secs: 1,
        });
        let resp2 = remap.handle_request_with_groups(&req, &[]);
        let ino2 = resp2.attr().unwrap().ino;

        assert_eq!(ino1, ino2, "same file should always get same stable inode");
    }

    #[test]
    fn test_hard_link_same_inode() {
        let mock = MockFs::new();
        let remap = RemapFs::new(mock);

        // Lookup "file_a" → inner_ino 42
        remap.inner.set_response(VolumeResponse::Entry {
            attr: make_attr(42),
            generation: 0,
            ttl_secs: 1,
        });
        let req_a = VolumeRequest::Lookup {
            parent: 1,
            name: b"file_a".to_vec(),
            uid: 0,
            gid: 0,
            pid: 0,
        };
        let resp_a = remap.handle_request_with_groups(&req_a, &[]);
        let stable_a = resp_a.attr().unwrap().ino;

        // Lookup "file_b" → same inner_ino 42 (hard link)
        remap.inner.set_response(VolumeResponse::Entry {
            attr: make_attr(42),
            generation: 0,
            ttl_secs: 1,
        });
        let req_b = VolumeRequest::Lookup {
            parent: 1,
            name: b"file_b".to_vec(),
            uid: 0,
            gid: 0,
            pid: 0,
        };
        let resp_b = remap.handle_request_with_groups(&req_b, &[]);
        let stable_b = resp_b.attr().unwrap().ino;

        assert_eq!(
            stable_a, stable_b,
            "hard links must share the same stable inode"
        );
    }

    #[test]
    fn test_getattr_remaps_response() {
        let mock = MockFs::new();
        let remap = RemapFs::new(mock);

        // First, register the inode via lookup
        remap.inner.set_response(VolumeResponse::Entry {
            attr: make_attr(200),
            generation: 0,
            ttl_secs: 1,
        });
        let lookup = VolumeRequest::Lookup {
            parent: 1,
            name: b"test".to_vec(),
            uid: 0,
            gid: 0,
            pid: 0,
        };
        let resp = remap.handle_request_with_groups(&lookup, &[]);
        let stable_ino = resp.attr().unwrap().ino;

        // Now getattr with stable inode
        remap.inner.set_response(VolumeResponse::Attr {
            attr: make_attr(200), // inner handler returns inner ino
            ttl_secs: 1,
        });
        let getattr = VolumeRequest::Getattr { ino: stable_ino };
        let resp = remap.handle_request_with_groups(&getattr, &[]);

        match resp {
            VolumeResponse::Attr { attr, .. } => {
                assert_eq!(attr.ino, stable_ino, "getattr should return stable inode");
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_readdir_remaps_known_entries() {
        let mock = MockFs::new();
        let remap = RemapFs::new(mock);

        // Register "file1" via lookup (inner_ino=10)
        remap.inner.set_response(VolumeResponse::Entry {
            attr: make_attr(10),
            generation: 0,
            ttl_secs: 1,
        });
        let lookup = VolumeRequest::Lookup {
            parent: 1,
            name: b"file1".to_vec(),
            uid: 0,
            gid: 0,
            pid: 0,
        };
        let resp = remap.handle_request_with_groups(&lookup, &[]);
        let stable_file1 = resp.attr().unwrap().ino;

        // Readdir on root
        remap.inner.set_response(VolumeResponse::DirEntries {
            entries: vec![
                DirEntry::dot(1),                        // inner root ino
                DirEntry::dotdot(1),                     // inner root parent
                DirEntry::new(10, b"file1".to_vec(), 8), // inner_ino=10
                DirEntry::new(20, b"file2".to_vec(), 8), // inner_ino=20, unknown
            ],
        });
        let readdir = VolumeRequest::Readdir {
            ino: 1, // root (stable = 1 = inner)
            offset: 0,
            uid: 0,
            gid: 0,
            pid: 0,
        };
        let resp = remap.handle_request_with_groups(&readdir, &[]);

        match resp {
            VolumeResponse::DirEntries { entries } => {
                assert_eq!(entries[0].ino, 1, ". should be root stable ino");
                assert_eq!(entries[1].ino, 1, ".. should be parent stable ino");
                assert_eq!(
                    entries[2].ino, stable_file1,
                    "file1 should use registered stable ino"
                );
                // file2 unknown: should get on-the-fly hash
                assert_eq!(
                    entries[3].ino,
                    RemapFs::<MockFs>::path_hash("file2"),
                    "unknown entry should get path hash"
                );
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_readdir_detects_stale_path() {
        let mock = MockFs::new();
        let remap = RemapFs::new(mock);

        // Register "old_name" via lookup (inner_ino=50)
        remap.inner.set_response(VolumeResponse::Entry {
            attr: make_attr(50),
            generation: 0,
            ttl_secs: 1,
        });
        let lookup = VolumeRequest::Lookup {
            parent: 1,
            name: b"old_name".to_vec(),
            uid: 0,
            gid: 0,
            pid: 0,
        };
        remap.handle_request_with_groups(&lookup, &[]);

        // Verify path is "old_name"
        let stable_50 = remap.to_stable(50).unwrap();
        assert_eq!(remap.paths.get(&stable_50).unwrap().value(), "old_name");

        // Host renames file. Readdir now shows inner_ino=50 as "new_name"
        remap.inner.set_response(VolumeResponse::DirEntries {
            entries: vec![DirEntry::new(50, b"new_name".to_vec(), 8)],
        });
        let readdir = VolumeRequest::Readdir {
            ino: 1,
            offset: 0,
            uid: 0,
            gid: 0,
            pid: 0,
        };
        remap.handle_request_with_groups(&readdir, &[]);

        // Path should be updated to "new_name"
        assert_eq!(
            remap.paths.get(&stable_50).unwrap().value(),
            "new_name",
            "stale path should be corrected by readdir"
        );
    }

    #[test]
    fn test_rename_updates_paths() {
        let mock = MockFs::new();
        let remap = RemapFs::new(mock);

        // Register "src" via lookup (inner_ino=100)
        remap.inner.set_response(VolumeResponse::Entry {
            attr: make_attr(100),
            generation: 0,
            ttl_secs: 1,
        });
        let lookup = VolumeRequest::Lookup {
            parent: 1,
            name: b"src".to_vec(),
            uid: 0,
            gid: 0,
            pid: 0,
        };
        remap.handle_request_with_groups(&lookup, &[]);

        let stable_100 = remap.to_stable(100).unwrap();
        assert_eq!(remap.paths.get(&stable_100).unwrap().value(), "src");

        // Guest renames "src" → "dst"
        remap.inner.set_response(VolumeResponse::Ok);
        let rename = VolumeRequest::Rename {
            parent: 1,
            name: b"src".to_vec(),
            newparent: 1,
            newname: b"dst".to_vec(),
            flags: 0,
            uid: 0,
            gid: 0,
            pid: 0,
        };
        remap.handle_request_with_groups(&rename, &[]);

        // Path should now be "dst"
        assert_eq!(
            remap.paths.get(&stable_100).unwrap().value(),
            "dst",
            "rename should update stored path"
        );
    }

    #[test]
    fn test_serialize_table() {
        let mock = MockFs::new();
        let remap = RemapFs::new(mock);

        // Register a file
        remap.inner.set_response(VolumeResponse::Entry {
            attr: make_attr(300),
            generation: 0,
            ttl_secs: 1,
        });
        let lookup = VolumeRequest::Lookup {
            parent: 1,
            name: b"data.txt".to_vec(),
            uid: 0,
            gid: 0,
            pid: 0,
        };
        remap.handle_request_with_groups(&lookup, &[]);

        let json = remap.serialize_table();
        let parsed: std::collections::BTreeMap<u64, String> = serde_json::from_str(&json).unwrap();

        // Should contain root ("") and "data.txt"
        assert_eq!(parsed.len(), 2);
        assert!(parsed.values().any(|v| v.is_empty()));
        assert!(parsed.values().any(|v| v == "data.txt"));
    }

    #[test]
    fn test_created_response_remapped() {
        let mock = MockFs::new();
        let remap = RemapFs::new(mock);

        // Create returns Created with inner_ino=77
        remap.inner.set_response(VolumeResponse::Created {
            attr: make_attr(77),
            generation: 0,
            ttl_secs: 1,
            fh: 99,
            flags: 0,
        });
        let create = VolumeRequest::Create {
            parent: 1,
            name: b"new_file".to_vec(),
            mode: 0o644,
            flags: 0,
            uid: 0,
            gid: 0,
            pid: 0,
        };
        let resp = remap.handle_request_with_groups(&create, &[]);

        match resp {
            VolumeResponse::Created { attr, fh, .. } => {
                assert_ne!(attr.ino, 77, "should not expose inner inode");
                assert_eq!(
                    attr.ino,
                    RemapFs::<MockFs>::path_hash("new_file"),
                    "should use path hash"
                );
                assert_eq!(fh, 99, "fh should pass through unchanged");
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_readdirplus_registers_mappings() {
        let mock = MockFs::new();
        let remap = RemapFs::new(mock);

        // Readdirplus returns entries
        remap.inner.set_response(VolumeResponse::DirEntriesPlus {
            entries: vec![DirEntryPlus {
                ino: 60,
                name: b"auto_reg".to_vec(),
                attr: make_attr(60),
                generation: 0,
                attr_ttl_secs: 1,
                entry_ttl_secs: 1,
            }],
        });

        let readdirplus = VolumeRequest::Readdirplus {
            ino: 1,
            fh: 0,
            offset: 0,
            uid: 0,
            gid: 0,
            pid: 0,
        };
        let resp = remap.handle_request_with_groups(&readdirplus, &[]);

        // Entry should be registered
        let stable = remap.to_stable(60);
        assert!(stable.is_some(), "readdirplus should register mapping");

        match resp {
            VolumeResponse::DirEntriesPlus { entries } => {
                assert_eq!(entries[0].ino, stable.unwrap());
                assert_eq!(entries[0].attr.ino, stable.unwrap());
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_error_response_passed_through() {
        let mock = MockFs::new();
        let remap = RemapFs::new(mock);

        remap
            .inner
            .set_response(VolumeResponse::error(libc::ENOENT));
        let req = VolumeRequest::Lookup {
            parent: 1,
            name: b"nonexistent".to_vec(),
            uid: 0,
            gid: 0,
            pid: 0,
        };
        let resp = remap.handle_request_with_groups(&req, &[]);
        assert_eq!(resp.errno(), Some(libc::ENOENT));
    }

    #[test]
    fn test_unknown_stable_ino_returns_eio() {
        let mock = MockFs::new();
        let remap = RemapFs::new(mock);

        // Getattr with unknown stable inode (never registered)
        let req = VolumeRequest::Getattr { ino: 999999 };
        let resp = remap.handle_request_with_groups(&req, &[]);
        assert_eq!(resp.errno(), Some(libc::EIO));
    }

    /// Mock handler that supports lookup chains for restore testing.
    /// Maps (parent_ino, name) → child_ino.
    struct LookupFs {
        entries: std::collections::HashMap<(u64, Vec<u8>), u64>,
    }

    impl LookupFs {
        fn new() -> Self {
            Self {
                entries: std::collections::HashMap::new(),
            }
        }

        fn add_entry(&mut self, parent: u64, name: &[u8], ino: u64) {
            self.entries.insert((parent, name.to_vec()), ino);
        }
    }

    impl FilesystemHandler for LookupFs {
        fn handle_request(&self, request: &VolumeRequest) -> VolumeResponse {
            match request {
                VolumeRequest::Lookup { parent, name, .. } => {
                    if let Some(&ino) = self.entries.get(&(*parent, name.clone())) {
                        VolumeResponse::Entry {
                            attr: make_attr(ino),
                            generation: 0,
                            ttl_secs: 1,
                        }
                    } else {
                        VolumeResponse::not_found()
                    }
                }
                _ => VolumeResponse::Ok,
            }
        }
    }

    #[test]
    fn test_serialize_restore_roundtrip() {
        let mock = MockFs::new();
        let remap = RemapFs::new(mock);

        // Register entries: root/dir/file.txt
        // First: lookup "dir" (inner_ino=10)
        remap.inner.set_response(VolumeResponse::Entry {
            attr: {
                let mut a = make_attr(10);
                a.mode = libc::S_IFDIR | 0o755;
                a
            },
            generation: 0,
            ttl_secs: 1,
        });
        let lookup_dir = VolumeRequest::Lookup {
            parent: 1,
            name: b"dir".to_vec(),
            uid: 0,
            gid: 0,
            pid: 0,
        };
        let resp = remap.handle_request_with_groups(&lookup_dir, &[]);
        let dir_stable = resp.attr().unwrap().ino;

        // Then: lookup "file.txt" under dir (inner_ino=20)
        remap.inner.set_response(VolumeResponse::Entry {
            attr: make_attr(20),
            generation: 0,
            ttl_secs: 1,
        });
        let lookup_file = VolumeRequest::Lookup {
            parent: dir_stable,
            name: b"file.txt".to_vec(),
            uid: 0,
            gid: 0,
            pid: 0,
        };
        let resp = remap.handle_request_with_groups(&lookup_file, &[]);
        let file_stable = resp.attr().unwrap().ino;

        // Serialize
        let json = remap.serialize_table();

        // Restore with a LookupFs that maps the same paths to different inner inodes
        let mut restore_fs = LookupFs::new();
        restore_fs.add_entry(1, b"dir", 500); // different inner ino
        restore_fs.add_entry(500, b"file.txt", 600); // different inner ino

        let restored = RemapFs::restore_from_table(restore_fs, &json);

        // The stable inodes should be the same
        assert_eq!(
            restored.to_inner(dir_stable),
            Some(500),
            "restored dir should map to new inner ino"
        );
        assert_eq!(
            restored.to_inner(file_stable),
            Some(600),
            "restored file should map to new inner ino"
        );

        // Reverse mappings should also work
        assert_eq!(restored.to_stable(500), Some(dir_stable));
        assert_eq!(restored.to_stable(600), Some(file_stable));

        // Paths should be preserved
        assert_eq!(restored.paths.get(&dir_stable).unwrap().value(), "dir");
        assert_eq!(
            restored.paths.get(&file_stable).unwrap().value(),
            "dir/file.txt"
        );
    }

    #[test]
    fn test_restore_missing_path_skipped() {
        // Serialize a table with a file that won't exist on restore
        let mock = MockFs::new();
        let remap = RemapFs::new(mock);

        remap.inner.set_response(VolumeResponse::Entry {
            attr: make_attr(42),
            generation: 0,
            ttl_secs: 1,
        });
        let lookup = VolumeRequest::Lookup {
            parent: 1,
            name: b"gone.txt".to_vec(),
            uid: 0,
            gid: 0,
            pid: 0,
        };
        remap.handle_request_with_groups(&lookup, &[]);
        let json = remap.serialize_table();

        // Restore with empty filesystem (no files)
        let restore_fs = LookupFs::new();
        let restored = RemapFs::restore_from_table(restore_fs, &json);

        // Root should exist, but gone.txt should be skipped
        assert!(restored.to_inner(1).is_some(), "root should exist");
        assert_eq!(
            restored.paths.len(),
            1,
            "only root should be in paths (gone.txt skipped)"
        );
    }

    #[test]
    fn test_rename_updates_descendant_paths() {
        let mock = MockFs::new();
        let remap = RemapFs::new(mock);

        // Register dir/child
        remap.inner.set_response(VolumeResponse::Entry {
            attr: {
                let mut a = make_attr(10);
                a.mode = libc::S_IFDIR | 0o755;
                a
            },
            generation: 0,
            ttl_secs: 1,
        });
        remap.handle_request_with_groups(
            &VolumeRequest::Lookup {
                parent: 1,
                name: b"dir".to_vec(),
                uid: 0,
                gid: 0,
                pid: 0,
            },
            &[],
        );
        let dir_stable = remap.to_stable(10).unwrap();

        remap.inner.set_response(VolumeResponse::Entry {
            attr: make_attr(20),
            generation: 0,
            ttl_secs: 1,
        });
        remap.handle_request_with_groups(
            &VolumeRequest::Lookup {
                parent: dir_stable,
                name: b"child.txt".to_vec(),
                uid: 0,
                gid: 0,
                pid: 0,
            },
            &[],
        );
        let child_stable = remap.to_stable(20).unwrap();

        assert_eq!(
            remap.paths.get(&child_stable).unwrap().value(),
            "dir/child.txt"
        );

        // Rename "dir" → "newdir"
        remap.inner.set_response(VolumeResponse::Ok);
        remap.handle_request_with_groups(
            &VolumeRequest::Rename {
                parent: 1,
                name: b"dir".to_vec(),
                newparent: 1,
                newname: b"newdir".to_vec(),
                flags: 0,
                uid: 0,
                gid: 0,
                pid: 0,
            },
            &[],
        );

        // Both parent and child paths should be updated
        assert_eq!(remap.paths.get(&dir_stable).unwrap().value(), "newdir");
        assert_eq!(
            remap.paths.get(&child_stable).unwrap().value(),
            "newdir/child.txt",
            "descendant path should be updated after directory rename"
        );
    }

    #[test]
    fn test_host_rename_then_serialize_accurate() {
        let mock = MockFs::new();
        let remap = RemapFs::new(mock);

        // Register "original" (inner_ino=50)
        remap.inner.set_response(VolumeResponse::Entry {
            attr: make_attr(50),
            generation: 0,
            ttl_secs: 1,
        });
        remap.handle_request_with_groups(
            &VolumeRequest::Lookup {
                parent: 1,
                name: b"original".to_vec(),
                uid: 0,
                gid: 0,
                pid: 0,
            },
            &[],
        );
        let stable_50 = remap.to_stable(50).unwrap();
        assert_eq!(remap.paths.get(&stable_50).unwrap().value(), "original");

        // Host renames file. Readdir now shows inner_ino=50 as "renamed"
        remap.inner.set_response(VolumeResponse::DirEntries {
            entries: vec![DirEntry::new(50, b"renamed".to_vec(), 8)],
        });
        remap.handle_request_with_groups(
            &VolumeRequest::Readdir {
                ino: 1,
                offset: 0,
                uid: 0,
                gid: 0,
                pid: 0,
            },
            &[],
        );

        // Path should be updated
        assert_eq!(remap.paths.get(&stable_50).unwrap().value(), "renamed");

        // Serialize should reflect the corrected path
        let json = remap.serialize_table();
        let parsed: std::collections::BTreeMap<u64, String> = serde_json::from_str(&json).unwrap();
        assert!(
            parsed.values().any(|v| v == "renamed"),
            "serialized table should have corrected path"
        );
        assert!(
            !parsed.values().any(|v| v == "original"),
            "serialized table should NOT have stale path"
        );
    }

    #[test]
    fn test_forget_remaps_inode() {
        let mock = MockFs::new();
        let remap = RemapFs::new(mock);

        // Register a file
        remap.inner.set_response(VolumeResponse::Entry {
            attr: make_attr(42),
            generation: 0,
            ttl_secs: 1,
        });
        remap.handle_request_with_groups(
            &VolumeRequest::Lookup {
                parent: 1,
                name: b"f".to_vec(),
                uid: 0,
                gid: 0,
                pid: 0,
            },
            &[],
        );
        let stable = remap.to_stable(42).unwrap();

        // Forget should not error (best-effort)
        remap.inner.set_response(VolumeResponse::Ok);
        let resp = remap.handle_request_with_groups(
            &VolumeRequest::Forget {
                ino: stable,
                nlookup: 1,
            },
            &[],
        );
        assert!(resp.is_ok());
    }
}
