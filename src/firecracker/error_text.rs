//! Firecracker's own error text that fcvm matches.
//!
//! fcvm decides what a failed snapshot load means from words in Firecracker's reply: whether
//! the snapshot can never load on this binary, and whether to tell the user to re-create it.
//! Firecracker has no error codes for these, and its wording changes (firecracker#6413 moved
//! every error from its Debug form to its Display form). So every piece of Firecracker's
//! text that fcvm looks for is a constant here, the code that matches takes it from here, and
//! `tests/test_firecracker_error_text.rs` reads the Firecracker binary a profile runs and
//! fails for each constant the binary does not contain. A pin whose build words one of them
//! differently fails that test instead of quietly no longer matching.

/// The prefix of every failed `PUT /snapshot/load`.
pub const LOAD_SNAPSHOT_ERROR: &str = "Load snapshot error";
/// Wraps every error of reading the state file: CRC, magic, version, decode, a short read.
pub const STATE_FILE_UNREADABLE: &str = "Failed to load snapshot state from file";
/// The state file is of a format or layout this build does not read. Builds of the fork
/// that mark a snapshot's layout say this for a file with no mark or another layout's, too.
pub const INVALID_DATA_VERSION: &str = "Invalid data version";
/// The state file's bytes do not decode as this build's state.
pub const STATE_DOES_NOT_DECODE: &str = "An error occurred during bitcode serialization";
/// The state decoded and failed its sanity check (no DRAM, too many slots).
pub const INVALID_SNAPSHOT_STATE: &str = "Invalid snapshot state";
/// A network override names a device the state does not hold.
pub const UNKNOWN_NETWORK_DEVICE: &str = "Unknown Network Device";
/// A vsock override was given and the state holds no vsock device.
pub const UNKNOWN_VSOCK_DEVICE: &str = "Unknown Vsock Device";
/// A drive override names a device the state does not hold.
pub const UNKNOWN_BLOCK_DEVICE: &str = "Unknown Block Device";
/// The state holds no TSC frequency.
pub const NO_TSC_FREQUENCY: &str = "frequency not present in the given snapshot";

/// Every constant above: what the pinned binary is checked for.
pub const ALL: &[&str] = &[
    LOAD_SNAPSHOT_ERROR,
    STATE_FILE_UNREADABLE,
    INVALID_DATA_VERSION,
    STATE_DOES_NOT_DECODE,
    INVALID_SNAPSHOT_STATE,
    UNKNOWN_NETWORK_DEVICE,
    UNKNOWN_VSOCK_DEVICE,
    UNKNOWN_BLOCK_DEVICE,
    NO_TSC_FREQUENCY,
];

#[cfg(test)]
mod tests {
    use super::ALL;

    /// Each constant is in `ALL`, so the binary check covers it. The list is read back from
    /// this file: a constant added above and left out of `ALL` fails here.
    #[test]
    fn every_constant_is_checked_against_the_binary() {
        let source = include_str!("error_text.rs");
        let declared: Vec<&str> = source
            .lines()
            .filter_map(|line| line.strip_prefix("pub const "))
            .filter(|rest| rest.contains(": &str = \""))
            .filter_map(|rest| rest.split('"').nth(1))
            .collect();
        assert!(declared.len() >= 9, "{declared:?}");
        for text in &declared {
            assert!(ALL.contains(text), "{text:?} is not in ALL");
        }
        assert_eq!(declared.len(), ALL.len());
    }
}
