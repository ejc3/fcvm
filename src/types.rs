use serde::{Deserialize, Serialize};

/// Execution mode for fcvm (privileged vs rootless)
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Auto,
    Privileged,
    Rootless,
}

/// Volume mapping mode
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MapMode {
    Block,
    Sshfs,
    Nfs,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mode_serialization() {
        let mode = Mode::Rootless;
        let json = serde_json::to_string(&mode).unwrap();
        assert_eq!(json, "\"rootless\"");

        let deserialized: Mode = serde_json::from_str(&json).unwrap();
        assert_eq!(mode, deserialized);
    }

    #[test]
    fn test_map_mode_serialization() {
        let map_mode = MapMode::Block;
        let json = serde_json::to_string(&map_mode).unwrap();
        assert_eq!(json, "\"block\"");

        let deserialized: MapMode = serde_json::from_str(&json).unwrap();
        assert_eq!(map_mode, deserialized);
    }
}
