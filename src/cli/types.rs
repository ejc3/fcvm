use super::args::{MapModeOpt, ModeOpt};
use crate::{MapMode, Mode};

impl From<ModeOpt> for Mode {
    fn from(m: ModeOpt) -> Self {
        match m {
            ModeOpt::Auto => Mode::Auto,
            ModeOpt::Privileged => Mode::Privileged,
            ModeOpt::Rootless => Mode::Rootless,
        }
    }
}

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
    fn test_mode_opt_conversion() {
        assert_eq!(Mode::from(ModeOpt::Auto), Mode::Auto);
        assert_eq!(Mode::from(ModeOpt::Privileged), Mode::Privileged);
        assert_eq!(Mode::from(ModeOpt::Rootless), Mode::Rootless);
    }

    #[test]
    fn test_map_mode_opt_conversion() {
        assert_eq!(MapMode::from(MapModeOpt::Block), MapMode::Block);
        assert_eq!(MapMode::from(MapModeOpt::Sshfs), MapMode::Sshfs);
        assert_eq!(MapMode::from(MapModeOpt::Nfs), MapMode::Nfs);
    }
}
