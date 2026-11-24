//! Read, before a snapshot is taken, the files a clone's restore path executes and maps, so
//! they are in the guest's page cache when its memory is captured.
//!
//! A restore restarts journald and chronyd and runs `systemctl` and `chronyc`. The snapshot's
//! guest has usually never run them, so every clone read them from its own disk: 8.0 MiB per
//! clone (`system.journal` 2.1 MiB, `systemctl` 1.4 MiB, `libcrypto`, `libsystemd-core`,
//! `chronyd`, ...), the same bytes in every clone, and each clone's disk is its own file on
//! the host. Read here, they come back with the snapshot's memory instead.
//!
//! This only reads. It starts, stops, flushes and writes nothing, so it is not a change to the
//! source VM (CLAUDE.md, "Suspending a VM does not disturb it"): a page-cache read is what any
//! process in the guest does, the kernel can drop the pages again at any time, and nothing
//! the guest runs can tell a cached page from one it reads later. It is a hint: a file that
//! cannot be read is skipped.
//!
//! The list is derived, not kept by hand: the programs are the ones the restore path spawns
//! ([`RESTORE_PROGRAMS`]; `restore_programs_cover_every_spawn_on_the_restore_path` fails when
//! a restore step spawns a program it does not list), the services are the units it restarts,
//! read from their unit files, and each executable brings its dynamic library closure, read
//! from its ELF headers.

use std::path::{Path, PathBuf};

// Absolute paths in the guest image (Ubuntu 24.04, where /bin is /usr/bin).

/// `systemctl`, which the restore runs to restart journald.
pub const SYSTEMCTL: &str = "/usr/bin/systemctl";
/// `chronyd`, which the restore starts again after stepping the clock.
pub const CHRONYD: &str = "/usr/sbin/chronyd";
/// `chronyc`, which the restore runs to wait for chronyd's sources.
pub const CHRONYC: &str = "/usr/bin/chronyc";
/// `umount` and `mount`, which the restore runs when the guest has NFS shares.
pub const UMOUNT: &str = "/usr/bin/umount";
pub const MOUNT: &str = "/usr/bin/mount";

/// Every program a restore spawns.
pub const RESTORE_PROGRAMS: &[&str] = &[SYSTEMCTL, CHRONYD, CHRONYC, UMOUNT, MOUNT];

/// The unit the restore restarts.
pub const JOURNALD_UNIT: &str = "systemd-journald";

/// What systemd runs for a unit it starts, besides the unit's own executable.
const SYSTEMD_EXECUTOR: &str = "/usr/lib/systemd/systemd-executor";
const UNIT_DIRS: &[&str] = &[
    "/etc/systemd/system",
    "/usr/lib/systemd/system",
    "/lib/systemd/system",
];
/// The journal files journald opens when it starts.
const JOURNAL_DIR: &str = "/var/log/journal";
const LIBRARY_DIRS: &[&str] = &[
    "/usr/lib/aarch64-linux-gnu",
    "/lib/aarch64-linux-gnu",
    "/usr/lib/x86_64-linux-gnu",
    "/lib/x86_64-linux-gnu",
    "/usr/lib",
    "/lib",
    "/lib64",
];

/// Read every file a restore executes or maps into the page cache. Returns the files read
/// and their total bytes.
pub fn warm() -> (usize, u64) {
    let mut files = Vec::new();
    let mut executables: Vec<PathBuf> = RESTORE_PROGRAMS.iter().map(PathBuf::from).collect();
    executables.push(PathBuf::from(SYSTEMD_EXECUTOR));
    executables.extend(unit_executables(JOURNALD_UNIT));
    if let Ok(me) = std::fs::read_link("/proc/self/exe") {
        executables.push(me);
    }
    for exe in executables {
        if !files.contains(&exe) && exe.is_file() {
            files.push(exe.clone());
        }
        for lib in library_closure(&exe) {
            if !files.contains(&lib) {
                files.push(lib);
            }
        }
    }
    files.extend(journal_files(Path::new(JOURNAL_DIR)));
    let bytes = files.iter().map(|f| read_through(f)).sum();
    (files.len(), bytes)
}

/// Read `path` to its end and discard it; the bytes read.
fn read_through(path: &Path) -> u64 {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return 0;
    };
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0;
    while let Ok(n) = file.read(&mut buf) {
        if n == 0 {
            break;
        }
        total += n as u64;
    }
    total
}

/// The executables a unit's `ExecStart=` lines name.
fn unit_executables(unit: &str) -> Vec<PathBuf> {
    UNIT_DIRS
        .iter()
        .map(|dir| Path::new(dir).join(format!("{unit}.service")))
        .find_map(|path| std::fs::read_to_string(path).ok())
        .map(|text| exec_start_paths(&text))
        .unwrap_or_default()
}

fn exec_start_paths(unit_text: &str) -> Vec<PathBuf> {
    unit_text
        .lines()
        .filter_map(|line| line.trim().strip_prefix("ExecStart="))
        .filter_map(|cmd| {
            cmd.trim_start_matches(['@', '-', ':', '+', '!'])
                .split_whitespace()
                .next()
                .map(PathBuf::from)
        })
        .filter(|path| path.is_absolute())
        .collect()
}

/// The `*.journal` files under `dir` and its machine-id subdirectories.
fn journal_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(journal_files(&path));
        } else if path.extension().is_some_and(|e| e == "journal") {
            out.push(path);
        }
    }
    out
}

/// The shared libraries `exe` needs, its interpreter included, followed transitively.
fn library_closure(exe: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut queue = vec![exe.to_path_buf()];
    while let Some(next) = queue.pop() {
        let Ok(bytes) = std::fs::read(&next) else {
            continue;
        };
        let Some(dynamic) = Dynamic::parse(&bytes) else {
            continue;
        };
        let mut found = Vec::new();
        if let Some(interp) = dynamic.interp {
            found.push(PathBuf::from(interp));
        }
        for name in &dynamic.needed {
            let dirs = dynamic
                .runpath
                .iter()
                .map(String::as_str)
                .chain(LIBRARY_DIRS.iter().copied());
            if let Some(path) = dirs.map(|d| Path::new(d).join(name)).find(|p| p.is_file()) {
                found.push(path);
            }
        }
        for path in found {
            if !out.contains(&path) {
                out.push(path.clone());
                queue.push(path);
            }
        }
    }
    out
}

/// What the dynamic linker reads from a 64-bit little-endian ELF file.
#[derive(Debug, Default, PartialEq)]
struct Dynamic {
    interp: Option<String>,
    needed: Vec<String>,
    runpath: Vec<String>,
}

impl Dynamic {
    fn parse(b: &[u8]) -> Option<Self> {
        const PT_LOAD: u32 = 1;
        const PT_DYNAMIC: u32 = 2;
        const PT_INTERP: u32 = 3;
        const DT_NEEDED: u64 = 1;
        const DT_STRTAB: u64 = 5;
        const DT_RPATH: u64 = 15;
        const DT_RUNPATH: u64 = 29;
        if b.get(0..4)? != b"\x7fELF" || *b.get(4)? != 2 || *b.get(5)? != 1 {
            return None;
        }
        let u16_at = |o: usize| Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?));
        let u32_at = |o: usize| Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?));
        let u64_at = |o: usize| Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?));
        let phoff = usize::try_from(u64_at(0x20)?).ok()?;
        let phentsize = usize::from(u16_at(0x36)?);
        let phnum = usize::from(u16_at(0x38)?);
        let mut loads = Vec::new();
        let (mut dynamic, mut interp) = (None, None);
        for i in 0..phnum {
            let ph = phoff.checked_add(i.checked_mul(phentsize)?)?;
            let (kind, offset, vaddr, filesz) = (
                u32_at(ph)?,
                u64_at(ph + 8)?,
                u64_at(ph + 16)?,
                u64_at(ph + 32)?,
            );
            match kind {
                PT_LOAD => loads.push((vaddr, offset, filesz)),
                PT_DYNAMIC => dynamic = Some((offset, filesz)),
                PT_INTERP => interp = Some((offset, filesz)),
                _ => {}
            }
        }
        let c_str = |off: usize| -> Option<String> {
            let rest = b.get(off..)?;
            let end = rest.iter().position(|&c| c == 0)?;
            Some(String::from_utf8_lossy(&rest[..end]).into_owned())
        };
        let mut out = Dynamic {
            interp: interp.and_then(|(off, _)| c_str(usize::try_from(off).ok()?)),
            ..Default::default()
        };
        let Some((dyn_off, dyn_size)) = dynamic else {
            return Some(out);
        };
        let mut entries = Vec::new();
        let mut strtab = None;
        let mut at = usize::try_from(dyn_off).ok()?;
        let end = at.checked_add(usize::try_from(dyn_size).ok()?)?;
        while at + 16 <= end {
            let (tag, val) = (u64_at(at)?, u64_at(at + 8)?);
            if tag == 0 {
                break;
            }
            if tag == DT_STRTAB {
                strtab = Some(val);
            }
            entries.push((tag, val));
            at += 16;
        }
        let strtab_vaddr = strtab?;
        let strtab_off = loads.iter().find_map(|&(vaddr, offset, filesz)| {
            (strtab_vaddr >= vaddr && strtab_vaddr < vaddr + filesz)
                .then(|| strtab_vaddr - vaddr + offset)
        })?;
        let name = |val: u64| c_str(usize::try_from(strtab_off.checked_add(val)?).ok()?);
        for (tag, val) in entries {
            match tag {
                DT_NEEDED => out.needed.extend(name(val)),
                DT_RPATH | DT_RUNPATH => {
                    if let Some(paths) = name(val) {
                        out.runpath.extend(paths.split(':').map(String::from));
                    }
                }
                _ => {}
            }
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The source of the free function `name` in `source`, to its closing brace.
    fn function<'a>(source: &'a str, name: &str) -> &'a str {
        let start = source
            .find(&format!("fn {name}("))
            .unwrap_or_else(|| panic!("no fn {name}"));
        let rest = &source[start..];
        let end = rest.find("\n}\n").unwrap();
        &rest[..end]
    }

    /// Every program a restore step spawns is one of [`RESTORE_PROGRAMS`], so the warm-up
    /// reads it: a step that spawns a program not listed fails this test until it is added.
    #[test]
    fn restore_programs_cover_every_spawn_on_the_restore_path() {
        let restore = include_str!("restore.rs");
        let restore = &restore[..restore.find("#[cfg(test)]").unwrap_or(restore.len())];
        let agent = include_str!("agent.rs");
        let mounts = include_str!("mounts.rs");
        let network = include_str!("snapshot_network.rs");
        let sites = [
            ("restore.rs", restore),
            ("agent::spawn_chronyd", function(agent, "spawn_chronyd")),
            ("agent::chronyc_sources", function(agent, "chronyc_sources")),
            (
                "mounts::mount_nfs_shares",
                function(mounts, "mount_nfs_shares"),
            ),
            ("snapshot_network::chronyc", function(network, "chronyc")),
        ];
        let mut spawns = 0;
        for (site, source) in sites {
            for (i, _) in source.match_indices("Command::new(\"") {
                let program = &source[i + "Command::new(\"".len()..];
                let program = &program[..program.find('"').unwrap()];
                let name = program.rsplit('/').next().unwrap();
                assert!(
                    RESTORE_PROGRAMS
                        .iter()
                        .any(|p| p.rsplit('/').next() == Some(name)),
                    "{site} spawns {program}, which RESTORE_PROGRAMS does not list"
                );
                spawns += 1;
            }
        }
        assert!(
            spawns >= 5,
            "found only {spawns} spawns on the restore path"
        );
        assert!(
            function(restore, "restart_journald").contains(&format!("\"{JOURNALD_UNIT}\"")),
            "the restore restarts a unit other than {JOURNALD_UNIT}"
        );
    }

    #[test]
    fn exec_start_lines_give_their_executables() {
        let unit = "[Service]\nExecStart=/usr/lib/systemd/systemd-journald\nExecStart=-@/bin/x y\nExecStart=relative\n";
        assert_eq!(
            exec_start_paths(unit),
            vec![
                PathBuf::from("/usr/lib/systemd/systemd-journald"),
                PathBuf::from("/bin/x")
            ]
        );
    }

    /// The closure of this test binary holds its C library, found through its ELF headers.
    #[test]
    fn a_dynamic_executable_brings_its_libraries() {
        let me = std::env::current_exe().unwrap();
        let bytes = std::fs::read(&me).unwrap();
        let dynamic = Dynamic::parse(&bytes).unwrap();
        if dynamic.needed.is_empty() {
            // A static build (musl) needs nothing: its closure is empty.
            assert!(library_closure(&me).is_empty());
            return;
        }
        assert!(
            dynamic.needed.iter().any(|n| n.starts_with("libc.so")),
            "{dynamic:?}"
        );
        let closure = library_closure(&me);
        assert!(
            closure.iter().any(|p| p
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("libc.so"))),
            "{closure:?}"
        );
    }

    #[test]
    fn a_file_that_is_not_elf_has_no_dynamic_section() {
        assert_eq!(Dynamic::parse(b"#!/bin/sh\n"), None);
        assert_eq!(Dynamic::parse(b""), None);
    }
}
