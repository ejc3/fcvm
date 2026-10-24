//! Every failpoint name, so a harness can enumerate them.
//!
//! `failpoint/tests/registry.rs` reads `src/` and `fc-agent/src/` and fails when a
//! `failpoint::hit*` call names a point missing here, or a point here has no call. A
//! deleted point therefore cannot leave a race gym replay that stops naming its window.

/// Points hit by fcvm on the host. Any of them can `park`.
pub const HOST_POINTS: &[&str] = &[
    "balloon.post_set_pre_report",
    "cache.miss",
    "cache.pre_ack",
    "exec.post_connect_pre_send",
    "fleet.agent.slot_running",
    "health.pre_persist_healthy",
    "lifecycle.before_ready_claim",
    "restore.locked",
    "restore.post_clone_disk",
    "restore.pre_spawn_vmm",
    "restore.pre_load",
    "restore.post_network_pre_resume",
    "restore.pre_resume",
    "restore.unlocked",
    "snapshot.create.locked",
    "snapshot.install.pre_rename",
    "snapshot.install.post_rename",
    "snapshot.post_balloon_read_pre_save",
    "snapshot.post_save_pre_resume",
    "snapshot.pre_pause",
    "teardown.pre_kill",
    "teardown.pre_remove_data_dir",
];

/// Points hit by fc-agent in the guest. They take `sleep` and `burn` only.
pub const GUEST_POINTS: &[&str] = &[
    "cache_ready.pre_send",
    "exec.post_accept_pre_read",
    "exec.post_ack_pre_go",
    "restore.pre_ack_connect",
];
