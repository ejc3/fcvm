mod handler;
mod server;

pub use handler::UffdHandler;
pub use server::{
    max_clones_per_server, preflight_clone_hugepages, UffdBacking, UffdServer,
    DEFAULT_MAX_CLONES_PER_SERVER, MAX_CLONES_ENV,
};
