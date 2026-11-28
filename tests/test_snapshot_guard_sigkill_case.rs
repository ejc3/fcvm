//! Source-level pin on the SIGKILL case in `tests/test_lifecycle_interleave.rs`.
//!
//! The case kills and reaps its create on every way out (a create left held at its failpoint
//! keeps the generation lock that the cleanup's delete waits on), takes the time of the kill
//! before the kill, and rejects an answer that arrives after the deadline.
//!
//! This lives here and not in `src/commands/snapshot_guard.rs`: an `include_str!` of a test
//! file from `src/` makes that file an input of the Lambda lane image
//! (`fleet/lambda/test_release.py` derives the inputs from the includes in `src/`), so every
//! edit to the integration test would rebuild an image that does not contain it. The file is
//! read at run time instead. It is not gated by `integration-fast`, so `make test-unit` runs it.

use std::path::PathBuf;

#[test]
fn the_sigkill_case_reaps_its_create_and_rejects_a_late_recovery() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/test_lifecycle_interleave.rs");
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let start = source
        .find("async fn killed_snapshot_create_resumes_its_source(")
        .expect("the SIGKILL case is gone from test_lifecycle_interleave.rs");
    let body = &source[start..start + source[start..].find("\n}\n").unwrap()];
    let killed_at = body.find("let killed_at = Instant::now();").unwrap();
    let kill = body.find("libc::SIGKILL").unwrap();
    assert!(killed_at < kill, "the kill's time is taken after the kill");
    assert!(body.contains("break in_time(\"exec\""));
    assert!(body.contains("break in_time(\"port\""));
    let after_block = &body[body.rfind("}\n    .await;").unwrap()..];
    let reaped = after_block
        .find("start_kill()")
        .expect("no kill of the create after the case");
    let stopped = after_block.find("sigterm(vm_pid)").unwrap();
    assert!(
        reaped < stopped,
        "the create is reaped after the VM is stopped"
    );
}
