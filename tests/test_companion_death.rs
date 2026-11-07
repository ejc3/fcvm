//! A companion that dies after the VM is up must end the VM with its own name.
//!
//! pasta and the rootless namespace holder run for the VM's whole life. Before
//! #852 nothing watched them once the VM was running: a SIGKILLed pasta left a
//! VM with no network that fcvm still reported healthy until its health check
//! failed, and the only errors were about whatever the absence broke next. Each
//! test here boots a rootless VM, kills one companion after it is healthy, and
//! requires fcvm to exit with an error naming that companion, its pid and its
//! exit status. The clone test does the same for a clone's pasta, which the
//! clone's own supervise loop watches.

#![cfg(feature = "privileged-tests")]

mod common;

use std::time::Duration;

/// Direct children of `pid` whose `comm` starts with `comm`. pasta runs as the
/// content-addressed `pasta-<sha>.bin`, so its comm is `pasta-` and 9 hex digits.
fn children_named(pid: u32, comm: &str) -> Vec<u32> {
    let mut found = Vec::new();
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return found;
    };
    for task in tasks.flatten() {
        let children = std::fs::read_to_string(task.path().join("children")).unwrap_or_default();
        for child in children
            .split_whitespace()
            .filter_map(|c| c.parse::<u32>().ok())
        {
            let name = std::fs::read_to_string(format!("/proc/{child}/comm")).unwrap_or_default();
            if name.trim().starts_with(comm) {
                found.push(child);
            }
        }
    }
    found
}

/// Kill `comm` under the healthy fcvm `child` and require fcvm to exit with an
/// error that names it as `reported_as`, with its pid and the signal.
async fn killing_a_companion_ends_it_naming_it(
    mut child: tokio::process::Child,
    fcvm_pid: u32,
    log_path: std::path::PathBuf,
    comm: &str,
    reported_as: &str,
) {
    let targets = children_named(fcvm_pid, comm);
    assert_eq!(
        targets.len(),
        1,
        "expected one {comm} child of fcvm {fcvm_pid}, found {targets:?}"
    );
    let target = targets[0];
    unsafe { libc::kill(target as i32, libc::SIGKILL) };

    let status = match tokio::time::timeout(Duration::from_secs(60), child.wait()).await {
        Ok(waited) => waited.expect("waiting on fcvm"),
        Err(_) => {
            let _ = child.kill().await;
            panic!("fcvm still running 60s after its {comm} (pid {target}) was killed");
        }
    };
    assert!(
        !status.success(),
        "a VM whose {comm} died must not exit cleanly"
    );

    let expected = format!("{reported_as} (pid {target}) exited: signal: 9");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut log_text = String::new();
    while std::time::Instant::now() < deadline {
        log_text = tokio::fs::read_to_string(&log_path)
            .await
            .unwrap_or_default();
        if log_text.contains(&expected) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let tail: String = log_text
        .chars()
        .rev()
        .take(4000)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    assert!(
        log_text.contains(&expected),
        "fcvm's error must name the dead companion as `{expected}`; log at {}:\n{tail}",
        log_path.display()
    );
}

/// A healthy rootless `podman run`.
async fn healthy_vm(name: &str) -> (tokio::process::Child, u32, std::path::PathBuf) {
    let (mut child, pid, log_path) = common::spawn_fcvm_with_log_path(
        &[
            "podman",
            "run",
            "--name",
            name,
            "--network",
            "rootless",
            common::TEST_IMAGE,
        ],
        name,
    )
    .await
    .expect("spawning fcvm");
    if let Err(error) = common::poll_health_by_pid(pid, 300).await {
        let _ = child.kill().await;
        panic!("VM never became healthy: {error:#}");
    }
    (child, pid, log_path)
}

#[tokio::test]
async fn a_killed_pasta_ends_the_vm_with_pastas_own_report() {
    let (name, _, _, _) = common::unique_names("companion-pasta");
    let (child, pid, log) = healthy_vm(&name).await;
    killing_a_companion_ends_it_naming_it(child, pid, log, "pasta", "pasta").await;
}

/// The holder is `unshare --user --net -- sleep infinity`, so its comm is `sleep`.
#[tokio::test]
async fn a_killed_namespace_holder_ends_the_vm_with_the_holders_report() {
    let (name, _, _, _) = common::unique_names("companion-holder");
    let (child, pid, log) = healthy_vm(&name).await;
    killing_a_companion_ends_it_naming_it(child, pid, log, "sleep", "namespace holder").await;
}

/// A clone runs its own pasta and holder, watched by the clone's own loop.
#[tokio::test]
async fn a_killed_pasta_ends_a_clone_with_pastas_own_report() {
    let (name, clone, snapshot, _) = common::unique_names("companion-clone");
    let (mut base, base_pid, _) = healthy_vm(&name).await;
    common::create_snapshot_by_pid(base_pid, &snapshot)
        .await
        .expect("snapshot create");
    common::kill_process(base_pid).await;
    let _ = base.wait().await;
    let (child, pid, log) = common::spawn_fcvm_with_log_path(
        &["snapshot", "run", "--snapshot", &snapshot, "--name", &clone],
        &clone,
    )
    .await
    .expect("spawning the clone");
    common::poll_health_by_pid(pid, 300)
        .await
        .expect("the clone never became healthy");
    killing_a_companion_ends_it_naming_it(child, pid, log, "pasta", "pasta").await;
}
