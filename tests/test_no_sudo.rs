//! Test that sudo is blocked in unprivileged tests.
//!
//! This test verifies that the no-sudo.sh wrapper is working correctly
//! and that unprivileged tests cannot escalate to root.

#![cfg(feature = "integration-fast")]
#![cfg(not(feature = "privileged-tests"))]

#[test]
fn test_sudo_is_blocked() {
    let output = std::process::Command::new("sudo")
        .arg("true")
        .output()
        .expect("failed to execute sudo");

    assert!(
        !output.status.success(),
        "sudo should be blocked in unprivileged tests"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not allowed"),
        "sudo should show 'not allowed' error, got: {}",
        stderr
    );
}
