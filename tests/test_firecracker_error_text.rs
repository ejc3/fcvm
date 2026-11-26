//! The Firecracker binary a profile runs says its errors in the words fcvm matches.
//!
//! fcvm classifies a failed snapshot load by text in Firecracker's reply
//! (`fcvm::firecracker::error_text`). Firecracker has no error codes for those cases and its
//! wording has changed between versions, so a pin that words one of them differently would
//! stop matching with no other test failing: a snapshot that can never load would be retried
//! on every run instead of replaced, and the re-create hint would not be printed. This reads
//! the binary setup built and looks for each string. A message with no arguments, or the
//! text in front of its first argument, is one contiguous string in the binary.

#![cfg(feature = "integration-fast")]

use fcvm::firecracker::error_text;

/// The Firecracker binary of kernel profile `name` on this architecture. A missing profile or
/// binary fails the test: this lane runs after `make setup-fcvm`.
async fn profile_binary(name: &str) -> Vec<u8> {
    let profile = fcvm::setup::get_kernel_profile(name)
        .unwrap()
        .unwrap_or_else(|| panic!("no kernel profile {name:?} for this architecture"));
    let path = fcvm::setup::get_firecracker_for_profile(&profile, name)
        .await
        .unwrap_or_else(|err| panic!("the {name} profile's Firecracker is not built: {err:#}"));
    std::fs::read(&path).unwrap_or_else(|err| panic!("reading {}: {err}", path.display()))
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

/// Every piece of Firecracker's error text fcvm matches is in the default profile's binary.
#[tokio::test]
async fn the_default_firecracker_says_every_error_fcvm_matches() {
    let binary = profile_binary("default").await;
    // The check can fail: a string no Firecracker says is not found.
    assert!(!contains(&binary, "Unknown Teapot Device"));
    assert!(!error_text::ALL.is_empty());
    let missing: Vec<&str> = error_text::ALL
        .iter()
        .copied()
        .filter(|text| !contains(&binary, text))
        .collect();
    assert!(
        missing.is_empty(),
        "the default profile's Firecracker does not contain {missing:?}: its wording changed, \
         so fcvm no longer recognises those errors. Update fcvm::firecracker::error_text to the \
         new wording (and what matches it), or keep the old wording in the fork"
    );
}
