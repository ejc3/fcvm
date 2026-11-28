pub mod manager;
pub mod types;
pub mod utils;

// Re-export all state types and functions for convenience
pub use manager::{owner_is_live, LoopbackLease, StateManager};
pub(crate) use manager::{process_is_live, written_before_this_boot};
pub use types::{
    present, truncate_id, Guest, HealthStatus, PooledNamespace, ProcessType, VmConfig, VmState,
    VmStatus,
};
pub use utils::{generate_vm_id, validate_vm_name};
