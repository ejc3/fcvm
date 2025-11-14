use serde::{Deserialize, Serialize};

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
    fn test_map_mode_serialization() {
        let map_mode = MapMode::Block;
        let json = serde_json::to_string(&map_mode).unwrap();
        assert_eq!(json, "\"block\"");

        let deserialized: MapMode = serde_json::from_str(&json).unwrap();
        assert_eq!(map_mode, deserialized);
    }
}
