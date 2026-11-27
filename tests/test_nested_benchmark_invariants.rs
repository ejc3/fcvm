//! Source-level tests of the nested benchmark in tests/test_kvm.rs. The nested
//! tests themselves need a host kernel with nested virtualization; these read
//! the script and the L1 launch and run the relevant shell lines without a VM.

use std::process::Command;

fn source() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/test_kvm.rs"))
        .expect("read nested benchmark source")
}

fn benchmark_script(source: &str) -> &str {
    source
        .split_once("fn benchmark_script() -> &'static str {")
        .expect("benchmark function")
        .1
        .split_once("r#\"")
        .expect("benchmark script start")
        .1
        .split_once("\"#")
        .expect("benchmark script end")
        .0
}

#[test]
fn nested_benchmark_l1_preserves_the_test_environment() {
    let source = source();
    let launch = source
        .split_once("async fn run_nested_n_levels(")
        .expect("nested benchmark helper")
        .1
        .split_once("let fcvm_cmd = format!(")
        .expect("L1 command")
        .1
        .split_once("let status =")
        .expect("L1 command execution")
        .0;
    assert!(
        launch.contains("./target/release/fcvm podman run") && !launch.contains("sudo "),
        "the privileged runner already supplies root; another sudo resets the environment and \
         drops FCVM_NO_SNAPSHOT, FCVM_DATA_DIR and FCVM_CONFIG_DIR:\n{launch}"
    );
}

#[test]
fn nested_benchmark_checks_the_default_gateway_and_fails_closed() {
    let source = source();
    let probe = benchmark_script(&source)
        .split_once("# Test 2:")
        .expect("end of gateway probe")
        .0;
    // Shell functions replace only the two network commands. No packets are sent.
    let script = format!(
        r#"
ip() {{ printf '%s\n' "$TEST_ROUTES"; return "$TEST_ROUTE_STATUS"; }}
ping() {{
    if [ "${{@: -1}}" != "192.0.2.1" ]; then
        echo "wrong gateway: $*" >&2
        return 97
    fi
    return "$TEST_PING_STATUS"
}}
{probe}
"#
    );
    for (routes, route_status, ping_status, succeeds) in [
        ("default via 192.0.2.1 dev eth0", "0", "0", true),
        ("", "0", "0", false),
        ("default dev eth0 scope link", "0", "0", false),
        ("default via 192.0.2.1 dev eth0", "2", "0", false),
        ("default via 192.0.2.1 dev eth0", "0", "1", false),
    ] {
        let output = Command::new("bash")
            .args(["-c", &script, "gateway-probe", "1"])
            .env("TEST_ROUTES", routes)
            .env("TEST_ROUTE_STATUS", route_status)
            .env("TEST_PING_STATUS", ping_status)
            .output()
            .expect("run extracted gateway probe");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.success(),
            succeeds,
            "routes={routes:?}, route_status={route_status}, ping_status={ping_status}\n{stdout}\n{stderr}"
        );
        assert_eq!(
            stdout.contains("GATEWAY_L1=OK gateway=192.0.2.1"),
            succeeds,
            "gateway verdict must match the measured route and exit status: {stdout}"
        );
    }
}

#[test]
fn nested_benchmark_small_reads_report_microseconds_per_operation() {
    let source = source();
    let calculation = benchmark_script(&source)
        .lines()
        .find(|line| line.trim_start().starts_with("READ_US="))
        .expect("small-read per-operation calculation");
    let output = Command::new("bash")
        .args([
            "-c",
            &format!(
                "START=1000000000\nEND=1166364000\n{calculation}\nprintf '%s\\n' \"$READ_US\""
            ),
        ])
        .output()
        .expect("run small-read calculation");
    assert!(output.status.success(), "{calculation}");
    // 100 reads in 166.364 ms are 1663 us each, not 166364.
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "1663",
        "FUSE_SMALLREAD is labelled us/op, so the total must be divided by the 100 reads: {calculation}"
    );
}
