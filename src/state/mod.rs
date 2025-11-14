pub mod manager;
pub mod types;
pub mod utils;

// Re-export all state types and functions for convenience
pub use manager::StateManager;
pub use types::{HealthStatus, VmConfig, VmState, VmStatus};
pub use utils::generate_vm_id;
