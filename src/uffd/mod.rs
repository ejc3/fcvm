mod handler;
mod prefetch;
mod server;
mod working_set;

pub use handler::UffdHandler;
pub use server::{preflight_clone_hugepages, Prefetch, UffdBacking, UffdServer};
/// Exported so integration tests can read back what a real restore recorded.
pub use working_set::WorkingSetStore;
