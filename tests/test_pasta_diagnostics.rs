//! A dead pasta must fail the VM with pasta's OWN error, not a downstream one.
//!
//! pasta creates its TAP device and writes its readiness PID file BEFORE it
//! binds forwarded ports, so a port conflict kills it after every readiness
//! signal fcvm watches. The next network-setup step then fails on the vanished
//! TAP ("Cannot find device pasta0") and, before the fix, the only line naming
//! the real cause ("Listen failed ... Address already in use") was dropped:
//! logged under a bare `pasta` target that every documented RUST_LOG filter
//! discards, and absent from the propagated error.
//!
//! The conflict here is a wildcard 0.0.0.0 listener, which collides with a
//! pasta forward on ANY per-VM loopback IP: exactly how a host daemon squatting
//! a port broke every `--publish <port>` VM on a shared dev box.

// Boots a real (rootless) VM, so it runs only in the suites that guarantee
// fcvm's assets exist -- the same gate every VM-booting test uses. Without it
// the unprivileged container suite fails at "Custom firecracker not found"
// before the port conflict under test is ever reached.
#![cfg(feature = "privileged-tests")]

mod common;

use std::time::Duration;

#[tokio::test]
async fn port_conflict_failure_names_pastas_own_error() {
    // Squat a wildcard port for the lifetime of the test. Binding port 0 lets
    // the kernel pick a free one, so parallel tests cannot collide with each
    // other or with real services.
    let squatter = std::net::TcpListener::bind("0.0.0.0:0").expect("bind squatter port");
    let port = squatter.local_addr().expect("squatter addr").port();

    let (name, _, _, _) = common::unique_names("pasta-diag");

    // Rootless run publishing the squatted port, through the common spawn
    // helper (pdeathsig, config setup, log consumers). Network setup fails
    // before the VM boots, so the image reference is never pulled; the run
    // must exit nonzero and the captured log must carry pasta's stderr, not
    // only the downstream bridge failure.
    let (mut child, _pid, log_path) = common::spawn_fcvm_with_log_path(
        &[
            "podman",
            "run",
            "--name",
            &name,
            "--publish",
            &format!("{port}:80"),
            "alpine:latest",
        ],
        &name,
    )
    .await
    .expect("spawning fcvm");

    // The failure path is seconds (4 pasta start attempts); the bound exists
    // so a regression that makes the run hang cannot eat the whole suite and
    // cannot leave the VM tree behind.
    let status = match tokio::time::timeout(Duration::from_secs(180), child.wait()).await {
        Ok(waited) => waited.expect("waiting on fcvm"),
        Err(_) => {
            let _ = child.kill().await;
            panic!("fcvm still running 180s after publishing a squatted port");
        }
    };
    assert!(
        !status.success(),
        "a squatted published port must fail the run"
    );

    // The log consumers drain the child's pipes asynchronously; give the
    // final lines a moment to land in the file.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut log_text = String::new();
    while std::time::Instant::now() < deadline {
        log_text = tokio::fs::read_to_string(&log_path)
            .await
            .unwrap_or_default();
        if log_text.contains("Address already in use") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        log_text.contains("Address already in use"),
        "the failure must carry pasta's own stderr (the port conflict), not only \
         a downstream symptom; log at {}:\n{}",
        log_path.display(),
        log_text
            .chars()
            .rev()
            .take(4000)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>()
    );
}
