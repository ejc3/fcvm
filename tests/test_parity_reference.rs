//! The parity test's reading of host podman (`tests/parity_reference/mod.rs`), with no
//! podman and no VM.

mod parity_reference;

use parity_reference::{
    own_error, parse_inspect, unshare_test_verdict, Inspected, Looked, Root, INSPECT_SEPARATOR,
};

/// What host podman answered in #944 for an image whose passwd file has the user.
const LOOKUP_FAILURE: &str =
    "Error: unable to find user nobody: no matching entries in passwd file\n";

/// One line of inspect output, the way `INSPECT_FORMAT` prints it.
fn inspect_line(status: &str, root: &str) -> String {
    format!("{status}{INSPECT_SEPARATOR}{root}\n")
}

#[test]
fn an_error_line_is_podmans_own_error() {
    let own = own_error(LOOKUP_FAILURE.as_bytes(), Some(255)).expect("podman's own error");
    assert!(own.contains("unable to find user nobody"), "{own}");
}

#[test]
fn a_warning_line_before_the_error_does_not_hide_it() {
    let stderr = format!(
        "time=\"2026-09-19T06:16:12Z\" level=warning msg=\"The cgroupv2 manager is set to systemd\"\n{LOOKUP_FAILURE}"
    );
    let own = own_error(stderr.as_bytes(), Some(255)).expect("podman's own error");
    assert!(own.contains("unable to find user nobody"), "{own}");
}

#[test]
fn an_ask_that_hit_the_case_timeout_is_podmans_own_error() {
    // `run` reports a client it had to kill at the deadline as no exit code.
    let own = own_error(b"", None).expect("a killed ask has no answer of the command's");
    assert!(own.contains("timeout"), "{own}");
}

#[test]
fn what_the_command_printed_is_not_podmans_error() {
    assert_eq!(own_error(b"sh: nope: not found\n", Some(127)), None);
    assert_eq!(own_error(b"", Some(0)), None);
    assert_eq!(own_error(b"", Some(1)), None);
}

#[test]
fn a_warning_line_alone_is_not_an_error() {
    let stderr = "time=\"2026-09-19T06:16:12Z\" level=warning msg=\"something\"\n";
    assert_eq!(own_error(stderr.as_bytes(), Some(0)), None);
}

#[test]
fn a_mounted_root_is_a_path() {
    let merged = "/var/lib/containers/storage/overlay/0123abcd/merged";
    assert_eq!(
        parse_inspect(&inspect_line("running", merged)),
        Some(Inspected {
            status: "running",
            root: Root::Path(merged),
        })
    );
}

#[test]
fn no_value_for_the_root_means_it_is_not_mounted() {
    // What podman 4.9 and 5.8 print for a running container whose mount record is gone.
    for printed in ["<no value>", ""] {
        assert_eq!(
            parse_inspect(&inspect_line("running", printed)),
            Some(Inspected {
                status: "running",
                root: Root::NotMounted,
            }),
            "printed: {printed:?}"
        );
    }
}

#[test]
fn a_root_with_a_space_in_it_is_kept_whole() {
    let merged = "/mnt/container store/overlay/0123abcd/merged";
    assert_eq!(
        parse_inspect(&inspect_line("running", merged)).map(|inspected| inspected.root),
        Some(Root::Path(merged))
    );
}

#[test]
fn unshare_test_answers_only_with_0_and_1() {
    assert_eq!(unshare_test_verdict(Some(0), b""), Looked::Present);
    assert_eq!(unshare_test_verdict(Some(1), b""), Looked::Absent);
}

#[test]
fn any_other_end_of_unshare_test_is_no_answer() {
    // 125, 126 and 127 are podman unshare's own failures, and no exit code is a timeout.
    for exit in [Some(125), Some(126), Some(127), Some(2), None] {
        let verdict = unshare_test_verdict(exit, b"Error: cannot set up namespace\n");
        assert!(
            matches!(verdict, Looked::CouldNotLook(_)),
            "exit {exit:?} gave {verdict:?}"
        );
    }
}
