//! Metrics collection for stress tests.

use metrics_util::debugging::{DebuggingRecorder, Snapshotter};
use std::sync::OnceLock;

static SNAPSHOTTER: OnceLock<Snapshotter> = OnceLock::new();

pub fn init() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    SNAPSHOTTER.set(snapshotter).ok();
    metrics::set_global_recorder(recorder).ok();
}
