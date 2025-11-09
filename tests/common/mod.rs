use std::path::PathBuf;
use tempfile::TempDir;

/// Creates a temporary directory for testing
pub fn temp_dir() -> TempDir {
    TempDir::new().expect("Failed to create temporary directory")
}

/// Creates a temporary state directory and returns its path
pub fn temp_state_dir() -> (TempDir, PathBuf) {
    let temp = temp_dir();
    let state_dir = temp.path().join("state");
    (temp, state_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_temp_dir_creation() {
        let temp = temp_dir();
        assert!(temp.path().exists());
    }

    #[test]
    fn test_temp_state_dir_creation() {
        let (_temp, state_dir) = temp_state_dir();
        assert!(state_dir.parent().unwrap().exists());
    }
}
