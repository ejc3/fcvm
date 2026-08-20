mod handler;
mod prefetch;
mod server;
mod working_set;

pub use handler::UffdHandler;
pub use server::{
    preflight_clone_hugepages, record_window_from_env, Prefetch, ServeShape, UffdBacking,
    UffdServer, DEFAULT_PREFETCH_RECORD_WINDOW,
};
/// Exported so integration tests can read back what a real restore recorded.
pub use working_set::WorkingSetStore;
