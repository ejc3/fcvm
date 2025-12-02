#![allow(clippy::print_stdout)]

#[path = "pjdfstest_common.rs"]
mod common;

fn main() {
    if !common::is_pjdfstest_installed() {
        eprintln!("SKIPPED: pjdfstest not installed");
        std::process::exit(0);
    }
    let ok = common::run_all(false, 32);
    std::process::exit(if ok { 0 } else { 1 });
}
