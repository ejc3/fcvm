pub mod types;
pub mod manager;
pub mod utils;

// Re-export all state types and functions for convenience
pub use types::{VmState, VmStatus, VmConfig};
pub use manager::StateManager;
pub use utils::generate_vm_id;
