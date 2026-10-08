mod handler;
mod holes;
mod prefetch;
mod release;
mod server;
mod warmup;
mod working_set;

pub use handler::UffdHandler;
pub use holes::for_each_data_run;
pub use release::{mark_in_use, release_idle_snapshots, Released};
pub use server::{
    preflight_clone_hugepages, record_window_from_env, FaultAround, Prefetch, ServeShape,
    UffdBacking, UffdServer, DEFAULT_PREFETCH_RECORD_WINDOW,
};
/// Exported so integration tests can read back what a real restore recorded.
pub use working_set::WorkingSetStore;
pub(crate) use working_set::GRANULE;
