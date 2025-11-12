// Integration tests for state management (implemented functionality)

mod common;

use fcvm::state::{StateManager, VmState, VmStatus};
use tempfile::TempDir;

#[tokio::test]
async fn test_full_vm_lifecycle() {
    let temp = TempDir::new().unwrap();
    let manager = StateManager::new(temp.path().to_path_buf());
    manager.init().await.unwrap();

    // Create a VM
    let vm = VmState::new("vm-123".to_string(), "nginx:latest".to_string(), 2, 512);

    // Save it
    manager.save_state(&vm).await.unwrap();

    // List VMs
    let vms = manager.list_vms().await.unwrap();
    assert_eq!(vms.len(), 1);
    assert_eq!(vms[0].vm_id, "vm-123");

    // Load it
    let loaded = manager.load_state("vm-123").await.unwrap();
    assert_eq!(loaded.vm_id, "vm-123");
    assert_eq!(loaded.config.image, "nginx:latest");

    // Delete it
    manager.delete_state("vm-123").await.unwrap();

    // Verify deleted
    let vms = manager.list_vms().await.unwrap();
    assert_eq!(vms.len(), 0);
}

#[tokio::test]
async fn test_multiple_vms() {
    let temp = TempDir::new().unwrap();
    let manager = StateManager::new(temp.path().to_path_buf());
    manager.init().await.unwrap();

    // Create multiple VMs
    for i in 1..=3 {
        let mut vm = VmState::new(format!("vm-{}", i), format!("nginx:{}", i), 2, 512);
        vm.name = Some(format!("web-{}", i));
        manager.save_state(&vm).await.unwrap();
    }

    // List all
    let vms = manager.list_vms().await.unwrap();
    assert_eq!(vms.len(), 3);

    // Verify each one
    for i in 1..=3 {
        let vm = manager.load_state(&format!("vm-{}", i)).await.unwrap();
        assert_eq!(vm.name, Some(format!("web-{}", i)));
    }
}

#[tokio::test]
async fn test_vm_status_transitions() {
    let temp = TempDir::new().unwrap();
    let manager = StateManager::new(temp.path().to_path_buf());
    manager.init().await.unwrap();

    let mut vm = VmState::new("vm-status".to_string(), "redis:latest".to_string(), 1, 256);

    // Initial state
    assert!(matches!(vm.status, VmStatus::Starting));
    manager.save_state(&vm).await.unwrap();

    // Update to running
    vm.status = VmStatus::Running;
    vm.pid = Some(1234);
    manager.save_state(&vm).await.unwrap();

    let loaded = manager.load_state("vm-status").await.unwrap();
    assert!(matches!(loaded.status, VmStatus::Running));
    assert_eq!(loaded.pid, Some(1234));
}
