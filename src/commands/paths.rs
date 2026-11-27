//! `fcvm paths`: the directories this binary keeps its state and data in.
//!
//! A tool outside fcvm (the benchmark harnesses under `bench/`) that reads VM state files
//! asks this command for the state directory instead of joining `state` to a data root of
//! its own, so a binary that keeps its state somewhere else is followed without a change to
//! the tool. Every value comes from a `crate::paths` function, and nothing here reads, lists
//! or creates VM state.
//!
//! The JSON object is a contract: fields are added, never renamed or removed.
//!
//! | field | what it is | who needs it |
//! |---|---|---|
//! | `state_dir` | `paths::state_dir()`, where VM state files live | the harnesses' state readers |
//! | `data_dir` | `paths::data_dir()`, the data root (`vm-disks`, `snapshots`) | harnesses that remove a killed clone's runtime directory or read a snapshot's files, and `reqbench.sh`'s default data root |

use anyhow::{Context, Result};
use std::path::Path;

use crate::cli::PathsArgs;
use crate::paths;

/// One directory as a string, refusing a path that is not UTF-8 rather than printing a lossy
/// one a reader would open as another path.
fn text(name: &str, path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_string)
        .with_context(|| format!("{name} {} is not UTF-8", path.display()))
}

/// The `fcvm paths --json` object for these directories.
fn record(state_dir: &Path, data_dir: &Path) -> Result<serde_json::Value> {
    Ok(serde_json::json!({
        "state_dir": text("state_dir", state_dir)?,
        "data_dir": text("data_dir", data_dir)?,
    }))
}

pub fn cmd_paths(args: PathsArgs) -> Result<()> {
    let state_dir = paths::state_dir();
    let data_dir = paths::data_dir();
    let line = if args.json {
        record(&state_dir, &data_dir)?.to_string()
    } else {
        format!(
            "state_dir\t{}\ndata_dir\t{}",
            text("state_dir", &state_dir)?,
            text("data_dir", &data_dir)?
        )
    };
    crate::utils::write_whole_line(&mut std::io::stdout(), &line)
        .context("writing the paths to stdout")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The object holds exactly the two directories, under their contract names.
    #[test]
    fn the_record_names_each_directory_it_is_given() {
        let value = record(Path::new("/example/state"), Path::new("/example")).unwrap();
        let object = value.as_object().unwrap();
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["data_dir", "state_dir"]);
        assert_eq!(object["state_dir"], "/example/state");
        assert_eq!(object["data_dir"], "/example");
    }

    /// A directory that is not UTF-8 is refused, not printed lossily.
    #[test]
    fn a_path_that_is_not_utf8_is_refused() {
        use std::os::unix::ffi::OsStrExt;
        let odd = Path::new(std::ffi::OsStr::from_bytes(b"/example/\xff"));
        let error = record(odd, Path::new("/example")).unwrap_err();
        assert!(format!("{error:#}").contains("not UTF-8"), "{error:#}");
    }

    /// The command takes each directory from its `paths` function and reads no VM state: the
    /// body of `cmd_paths` calls `paths::state_dir()` and `paths::data_dir()`, and nothing in
    /// this module's code names the state store.
    #[test]
    fn the_command_reads_the_path_functions_and_no_state() {
        let source = include_str!("paths.rs");
        let code = &source[..source.find("#[cfg(test)]").unwrap()];
        let body = &code[code.find("pub fn cmd_paths(").unwrap()..];
        assert!(body.contains("paths::state_dir()"), "{body}");
        assert!(body.contains("paths::data_dir()"), "{body}");
        for name in [
            "StateManager",
            "list_vms",
            "load_state",
            "read_dir",
            "create_dir",
        ] {
            assert!(!code.contains(name), "the paths command names {name}");
        }
    }
}
