#![allow(clippy::print_stdout)]
#[path = "pjdfstest_common.rs"]
mod common;

fn main() {
    let ok = common::run_all(true, 256);
    std::process::exit(if ok { 0 } else { 1 });
}
