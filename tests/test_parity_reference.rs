//! The parity test's reading of host podman (`tests/parity_reference/mod.rs`), with no
//! podman and no VM.

mod parity_reference;

use parity_reference::{
    host_looked, mount_record_entry, mount_record_path, own_error, parse_inspect, render_probe,
    shell_words, shown_record, unshare_test_verdict, Inspected, Looked, Root, INSPECT_FORMAT,
    INSPECT_SEPARATOR,
};
use std::path::Path;
use std::time::Duration;

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

#[test]
fn the_inspect_format_puts_the_separator_between_its_two_fields() {
    assert_eq!(
        INSPECT_FORMAT,
        format!("{{{{.State.Status}}}}{INSPECT_SEPARATOR}{{{{.GraphDriver.Data.MergedDir}}}}")
    );
}

#[test]
fn inspect_output_without_the_separator_is_not_parsed() {
    assert_eq!(parse_inspect("running\n"), None);
    assert_eq!(parse_inspect(""), None);
}

#[test]
fn a_root_with_the_separator_in_it_is_kept_whole() {
    let merged = "/mnt/a|b/overlay/0123abcd/merged";
    assert_eq!(
        parse_inspect(&inspect_line("running", merged)),
        Some(Inspected {
            status: "running",
            root: Root::Path(merged),
        })
    );
}

#[test]
fn a_container_that_is_not_running_keeps_its_state() {
    assert_eq!(
        parse_inspect(&inspect_line("created", "<no value>")),
        Some(Inspected {
            status: "created",
            root: Root::NotMounted,
        })
    );
}

#[test]
fn a_root_that_is_no_path_is_shown_as_printed() {
    assert_eq!(
        parse_inspect(&inspect_line("running", "overlay")).map(|inspected| inspected.root),
        Some(Root::Unexpected("overlay"))
    );
}

#[test]
fn a_missing_file_is_absent_and_any_other_error_is_no_answer() {
    let dir = tempfile::TempDir::new().unwrap();
    let file = dir.path().join("passwd");
    std::fs::write(&file, "nobody:x:65534:65534::/:/sbin/nologin\n").unwrap();

    assert_eq!(host_looked(&file), Looked::Present);
    assert_eq!(host_looked(&dir.path().join("absent")), Looked::Absent);
    // A file where a directory belongs: the lookup fails with ENOTDIR, not ENOENT.
    let through_a_file = host_looked(&file.join("etc/passwd"));
    assert!(
        matches!(through_a_file, Looked::CouldNotLook(_)),
        "{through_a_file:?}"
    );
}

#[test]
fn a_dangling_symlink_is_present() {
    // An image's /etc/passwd may be an absolute symlink, which the host cannot follow.
    let dir = tempfile::TempDir::new().unwrap();
    let link = dir.path().join("passwd");
    std::os::unix::fs::symlink("/no/such/target", &link).unwrap();
    assert_eq!(host_looked(&link), Looked::Present);
}

#[test]
fn the_mount_record_is_under_the_runroot_by_driver() {
    assert_eq!(
        mount_record_path("/run/containers/storage|overlay\n").as_deref(),
        Some(Path::new(
            "/run/containers/storage/overlay-layers/mountpoints.json"
        ))
    );
    assert_eq!(
        mount_record_path("/run/user/1000/a|b|overlay\n").as_deref(),
        Some(Path::new(
            "/run/user/1000/a|b/overlay-layers/mountpoints.json"
        ))
    );
}

#[test]
fn store_output_that_names_no_runroot_gives_no_record_path() {
    for printed in [
        "",
        "overlay\n",
        "relative/run|overlay\n",
        "/run/containers/storage|\n",
    ] {
        assert_eq!(mount_record_path(printed), None, "printed: {printed:?}");
    }
}

/// A record of two mounted layers, as containers/storage writes it.
const RECORD: &str = r#"[{"id":"aaaa","path":"/store/overlay/aaaa/merged","count":1},{"id":"bbbb","path":"/store/overlay/bbbb/merged","count":2}]"#;

#[test]
fn the_record_entry_for_a_root_gives_its_layer_and_count() {
    let entry = mount_record_entry(RECORD, "/store/overlay/bbbb/merged");
    assert!(
        entry.contains("bbbb") && entry.contains("count 2"),
        "{entry}"
    );
}

#[test]
fn a_record_without_the_root_says_so() {
    let entry = mount_record_entry(RECORD, "/store/overlay/cccc/merged");
    assert!(entry.contains("no layer mounted there"), "{entry}");
    assert!(entry.contains("2 mounted elsewhere"), "{entry}");
    // What a store that does not know the layer leaves behind.
    let emptied = mount_record_entry("[]", "/store/overlay/cccc/merged");
    assert!(emptied.contains("0 mounted elsewhere"), "{emptied}");
}

#[test]
fn a_record_that_is_not_json_says_so() {
    let entry = mount_record_entry("not json", "/store/overlay/aaaa/merged");
    assert!(entry.contains("not the JSON"), "{entry}");
}

#[test]
fn a_short_record_is_shown_whole_and_a_long_one_is_cut() {
    assert_eq!(shown_record("[]\n"), "[]");
    let long = "x".repeat(5000);
    let shown = shown_record(&long);
    assert!(shown.len() < 2100, "{}", shown.len());
    assert!(
        shown.ends_with("(5000 characters, first 2000 shown)"),
        "{shown}"
    );
}

#[test]
fn a_command_in_the_failure_text_can_be_pasted_into_a_shell() {
    assert_eq!(
        shell_words(&["podman", "inspect", "--format", INSPECT_FORMAT, "ref-1"]),
        format!("podman inspect --format '{INSPECT_FORMAT}' ref-1")
    );
    assert_eq!(
        shell_words(&["grep", "-c", "^nobody:", "/etc/passwd"]),
        "grep -c '^nobody:' /etc/passwd"
    );
    assert_eq!(shell_words(&["echo", "", "it's"]), r"echo '' 'it'\''s'");
}

#[test]
fn a_probe_that_gave_no_answer_says_so() {
    let text = render_probe(
        "podman inspect ref",
        None,
        Duration::from_secs(20),
        b"",
        b"",
    );
    assert_eq!(text, "$ podman inspect ref\n  no answer in 20s, killed\n");
}

#[test]
fn a_probe_shows_its_exit_and_the_head_of_each_stream() {
    let text = render_probe(
        "podman exec ref cat /etc/passwd",
        Some(0),
        Duration::from_secs(20),
        b"one\ntwo\nthree\nfour\nfive\n",
        b"time=\"x\" level=warning msg=\"y\"\n",
    );
    assert_eq!(
        text,
        "$ podman exec ref cat /etc/passwd\n  exit 0\n  stdout | one\n  stdout | two\n  stdout | three\n  stdout | (5 lines, first 3 shown)\n  stderr | time=\"x\" level=warning msg=\"y\"\n"
    );
}
