pub mod cli;
pub mod commands;
pub mod firecracker;
pub mod health;
pub mod network;
pub mod paths;
pub mod setup;
pub mod state;
pub mod storage;
pub mod uffd;
pub mod utils;
pub mod volume;

/// Get total host memory in MiB from /proc/meminfo.
pub fn host_memory_mib() -> Option<u32> {
    std::fs::read_to_string("/proc/meminfo").ok().and_then(|s| {
        s.lines()
            .find(|l| l.starts_with("MemTotal:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|kb| kb.parse::<u64>().ok())
            .map(|kb| (kb / 1024) as u32)
    })
}
