use super::args::MapModeOpt;
use crate::MapMode;

impl From<MapModeOpt> for MapMode {
    fn from(m: MapModeOpt) -> Self {
        match m {
            MapModeOpt::Block => MapMode::Block,
            MapModeOpt::Sshfs => MapMode::Sshfs,
            MapModeOpt::Nfs => MapMode::Nfs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_map_mode_opt_conversion() {
        assert_eq!(MapMode::from(MapModeOpt::Block), MapMode::Block);
        assert_eq!(MapMode::from(MapModeOpt::Sshfs), MapMode::Sshfs);
        assert_eq!(MapMode::from(MapModeOpt::Nfs), MapMode::Nfs);
    }
}
