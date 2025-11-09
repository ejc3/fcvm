use uuid::Uuid;

/// Generate a new VM ID
pub fn generate_vm_id() -> String {
    format!("vm-{}", Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_vm_id_format() {
        let id = generate_vm_id();
        assert!(id.starts_with("vm-"));
        assert_eq!(id.len(), 35); // "vm-" (3) + 32 hex chars
    }

    #[test]
    fn test_generate_vm_id_unique() {
        let id1 = generate_vm_id();
        let id2 = generate_vm_id();
        assert_ne!(id1, id2);
    }
}
