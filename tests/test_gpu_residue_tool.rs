//! Source-level checks of the GPU residue test (scripts/gpu-residue), which
//! needs GPU hardware to run.

fn read(path: &str) -> String {
    std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(path))
        .unwrap_or_else(|e| panic!("reading {path}: {e}"))
}

/// The scan's control writes one marker and expects the count to rise by one,
/// unless the word it overwrote already held the marker: with residue present
/// that word can be a previous tenant's, and the control must not then abort
/// the measurement before it reports.
#[test]
fn the_scan_control_accounts_for_the_word_it_overwrites() {
    let source = read("scripts/gpu-residue/residue.cu");
    let control = &source[source
        .find("Positive control")
        .expect("residue.cu has no positive control")..];
    let read_back = control
        .find("cudaMemcpyDeviceToHost")
        .expect("the control does not read the word it overwrites");
    let write = control
        .find("cudaMemcpyHostToDevice")
        .expect("the control does not write its marker");
    assert!(
        read_back < write,
        "the word must be read before it is overwritten"
    );
    assert!(
        control.contains("!= MARKER"),
        "the expected count must depend on whether the old word was the marker"
    );
}

/// A run that stops early (A never reports FILLED, a failed scan) must not
/// leave VM A holding the GPU for the next run.
#[test]
fn the_runner_stops_vm_a_on_every_exit() {
    let script = read("scripts/gpu-residue/run.sh");
    let trap = script
        .lines()
        .find(|line| line.trim_start().starts_with("trap "))
        .expect("run.sh sets no trap");
    assert!(trap.contains("EXIT"), "the trap must run on exit: {trap}");
    let stop = &script[script.find("stop_a()").expect("run.sh has no stop_a")..];
    assert!(
        stop.contains("kill -9") && stop.contains("wait"),
        "stop_a must kill and reap VM A"
    );
}
