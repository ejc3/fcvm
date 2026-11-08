pub mod failure;
mod handler;
mod holes;
mod prefetch;
mod release;
mod server;
mod warmup;
mod working_set;

pub use handler::UffdHandler;
pub(crate) use holes::flush_for_hole_walk;
pub use holes::{for_each_data_run, try_for_each_data_run};
pub(crate) use release::staging_tag;
pub use release::{mark_in_use, release_idle_snapshots, release_unless_in_use, Released};
pub use server::{
    preflight_clone_hugepages, record_window_from_env, FaultAround, Prefetch, ReplayLimit,
    ReplayThreads, ServeShape, UffdBacking, UffdServer, DEFAULT_PREFETCH_RECORD_WINDOW,
};
/// The page cache helpers of the warm-up's tests, for tests outside this module that drop a
/// memory file from the page cache.
#[cfg(test)]
pub(crate) use warmup::testing as page_cache_testing;
/// Exported so integration tests can read back what a real restore recorded.
pub use working_set::WorkingSetStore;
pub(crate) use working_set::GRANULE;
pub(crate) use working_set::VERSION as WORKING_SET_VERSION;
/// What `snapshot import` needs to move an archived working set to the installed image's key.
pub(crate) use working_set::{max_recorded_len, ImageKey, PageSet};
