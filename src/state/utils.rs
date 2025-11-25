use anyhow::{bail, Result};
use uuid::Uuid;

/// Generate a new VM ID
pub fn generate_vm_id() -> String {
    format!("vm-{}", Uuid::new_v4().simple())
}

/// Validate a VM name for safe use in paths, network interfaces, and shell commands
///
/// Constraints:
/// - Must be 1-63 characters (network interface names have 15 char limit, but TAP uses prefix)
/// - Must contain only alphanumeric chars, dashes, or underscores
/// - Must not start with a dash
///
/// These rules prevent:
/// - Path traversal attacks (no slashes, dots)
/// - Shell injection (no special characters)
/// - Network interface name issues (reasonable length)
pub fn validate_vm_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("VM name cannot be empty");
    }

    if name.len() > 63 {
        bail!("VM name must be 63 characters or less, got {}", name.len());
    }

    if name.starts_with('-') {
        bail!("VM name cannot start with a dash");
    }

    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!(
            "VM name must contain only alphanumeric characters, dashes, or underscores: {:?}",
            name
        );
    }

    Ok(())
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

    #[test]
    fn test_validate_vm_name_valid() {
        assert!(validate_vm_name("my-vm").is_ok());
        assert!(validate_vm_name("my_vm").is_ok());
        assert!(validate_vm_name("myvm123").is_ok());
        assert!(validate_vm_name("VM-Test_123").is_ok());
        assert!(validate_vm_name("a").is_ok());
    }

    #[test]
    fn test_validate_vm_name_empty() {
        assert!(validate_vm_name("").is_err());
    }

    #[test]
    fn test_validate_vm_name_too_long() {
        let long_name = "a".repeat(64);
        assert!(validate_vm_name(&long_name).is_err());
        // 63 chars should be ok
        let max_name = "a".repeat(63);
        assert!(validate_vm_name(&max_name).is_ok());
    }

    #[test]
    fn test_validate_vm_name_starts_with_dash() {
        assert!(validate_vm_name("-myvm").is_err());
    }

    #[test]
    fn test_validate_vm_name_invalid_chars() {
        assert!(validate_vm_name("my/vm").is_err()); // path traversal
        assert!(validate_vm_name("my..vm").is_err()); // dots
        assert!(validate_vm_name("my vm").is_err()); // space
        assert!(validate_vm_name("my;vm").is_err()); // shell injection
        assert!(validate_vm_name("$(whoami)").is_err()); // command substitution
        assert!(validate_vm_name("my\0vm").is_err()); // null byte
    }
}
