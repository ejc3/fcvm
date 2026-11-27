//! A start lists no directory whose entry count grows with the VMs alive, anywhere on the host.
//!
//! Six times a start listed a directory that holds an entry for every VM alive: the state
//! directory under the name lock, the reclaim owner records, `vm-disks` for a mount view
//! nothing used, the parent cgroup of the per-VM scopes (#2178), and the two this test
//! found, `/run/netns` (#2194) and the routed address leases (#2202). Each cost nothing
//! beside one VM and bound the start rate beside a thousand, or would have, because the
//! listing is read under a lock that every other start's create in that directory needs.
//!
//! The test names no directory. It runs a start under `strace`, which follows every process
//! the start creates (fcvm, the namespace holder, pasta, the VMM), prints the path behind the
//! descriptor of each `getdents64` and the number of entries the call returned. A clone is
//! started beside one clone and again beside six, and the entries each start read are summed
//! per directory. A directory fails when the second start read at least one entry more from
//! it for each clone added. That rule holds on any filesystem: the store, cgroupfs, the
//! runtime directory, `/proc`.
//!
//! How directories are matched between the two starts:
//!
//! - The private data directory and the scopes' parent cgroup are named by the test, so a
//!   path under one is compared as `<data>/...` or `<scopes>/...`.
//! - A numeric component under `/proc` is a process or thread id and is compared as `<pid>`,
//!   so `/proc/<pid>/fd` is one directory whichever processes listed theirs.
//! - A directory that only one of the two starts listed (the VM's own directories, which
//!   carry its id) has no partner. Those are summed on each side and compared as one. A
//!   start that visits a directory of every sibling fails there.
//!
//! The threshold is one entry per clone added, not zero, because `/proc` and other host-wide
//! directories also follow processes this test did not start. A scan of `/proc` reads
//! several entries for each VM alive and fails. One process that came or went between the
//! two starts does not.
//!
//! What the test does not see:
//!
//! - A directory that gains less than one entry per VM, such as one bucket of a hashed
//!   layout. Five added clones do not show it.
//! - Work of processes the start did not create: a `fcvm netpool serve`, a `fcvm snapshot
//!   serve`. The measured start is `snapshot run --snapshot`, which has neither.
//! - Per-VM work that is not a directory listing.
//!
//! A listing that follows the VMs alive may run once an interval (a sweep, `fcvm::sweep`),
//! not once a start. The measured starts must not be the ones that sweep, so the test holds
//! off the sweeps it knows by name and makes every other sweep run in every start: a sweep
//! added later fails here until it is named.
//!
//! The starts are traced by root (`make test-root`): root's own, which enter a cgroup scope,
//! with rootless, bridged and routed networking, and an unprivileged user's. An unprivileged
//! strace cannot trace a rootless start as it runs untraced, see the `start` module.
//!
//! The same traces hold every call that can create a directory entry (mkdir, an open with
//! O_CREAT, a rename's target, link, symlink, mknod). Each takes the directory's inode lock
//! for writing, and on btrfs the holder can wait out a transaction commit (a clone's disk is
//! a subvolume snapshot, each ending in one), so a store directory that every start writes
//! into makes the starts take turns however few entries it holds: measured on fcvm's host
//! kernel, `vm-disks`, where every start makes its runtime directory, held 968 of 1411 ms of
//! lock wait per clone at 512 starts in flight. So every start is traced, the measured two
//! and the four added beside them, and a directory under the data directory that each of
//! the six created an entry in, and that is not a VM's own (no component is a VM id), fails.
//! A hashed bucket of 16 is in every one of six starts with odds of 16^-5. Making a bucket
//! (an entry named by one or two hex digits, or that name staged under a dotted temporary
//! name) happens once in a store's life and is not counted.
//!
//! `strace` is a requirement. Without it, or with one that lacks an option used here, every
//! test in this file that needs it fails: a detector that cannot run has seen nothing.

mod common;

use anyhow::{bail, ensure, Context, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// The entries read from one directory, and the commands whose processes read them.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct DirRead {
    entries: u64,
    readers: BTreeSet<String>,
}

/// What strace logged of one process tree.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Trace {
    /// Entries read per directory, by the path strace printed for the descriptor.
    dirs: BTreeMap<String, DirRead>,
    /// Each program a process became, in the order of the log.
    execs: Vec<(u32, String)>,
    /// Entries read through descriptors strace printed no path for.
    unnamed_entries: u64,
    /// Entry-creating calls that succeeded (a directory made, a file opened with O_CREAT, a
    /// rename's target, a link, a symlink, a node), by the directory the entry is in.
    creates: BTreeMap<String, u64>,
    /// Such calls whose directory strace's log does not name (a relative path of a call
    /// that takes no directory descriptor).
    unplaced_creates: u64,
}

/// A call strace logged the beginning of, by the process that is still in it.
enum Pending {
    Listing(Option<String>),
    Exec(String),
    /// A call that may create a directory entry: where it lands, if it creates one.
    Create(Option<Placed>),
}

/// The directory an entry-creating call puts its entry in, or that the log does not say, or
/// that the entry is a bucket of a hashed layout ([`makes_a_bucket`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Placed {
    In(String),
    Bucket,
    Unplaced,
}

/// Whether an entry named `name` is a bucket of a hashed layout: one or two lowercase hex
/// digits (`vm-disks/<b>`, `state/vms/<b>`), or such a name staged under a temporary dotted
/// name before it is renamed into place (`.7f.<uuid>`, `.index-staging.4.<uuid>`). A store
/// makes each bucket once in its life, not once a start, so making one is not counted.
fn makes_a_bucket(name: &str) -> bool {
    let hex = |part: &str, len: std::ops::RangeInclusive<usize>| {
        len.contains(&part.len())
            && part
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-')
    };
    if hex(name, 1..=2) {
        return true;
    }
    let Some(staged) = name.strip_prefix('.') else {
        return false;
    };
    let parts: Vec<&str> = staged.split('.').collect();
    parts.len() >= 2 && hex(parts[parts.len() - 1], 32..=36) && hex(parts[parts.len() - 2], 1..=2)
}

/// What the parser knows of the processes in the log so far.
#[derive(Default)]
struct Processes {
    pending: HashMap<u32, Pending>,
    /// The program each process last became.
    program: HashMap<u32, String>,
}

impl Processes {
    fn became(&mut self, pid: u32, program: String, trace: &mut Trace) {
        let name = Path::new(&program).file_name().map_or_else(
            || program.clone(),
            |name| name.to_string_lossy().into_owned(),
        );
        self.program.insert(pid, name);
        trace.execs.push((pid, program));
    }

    /// Who read a directory: the program the process became, if the log shows it. The
    /// command name strace prints can still be the parent's just after an exec. A thread,
    /// or a child that has not exec'd, is named by its command name.
    fn reader(&self, pid: u32, comm: &str) -> String {
        match self.program.get(&pid) {
            Some(program) => program.clone(),
            None if comm.is_empty() => String::from("?"),
            None => comm.to_string(),
        }
    }
}

const UNFINISHED: &str = "<unfinished ...>";

/// The bytes strace's escapes in a path or string stand for.
fn unescape(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'\\' || at + 1 == bytes.len() {
            out.push(bytes[at]);
            at += 1;
            continue;
        }
        let named = match bytes[at + 1] {
            b'n' => Some(b'\n'),
            b't' => Some(b'\t'),
            b'r' => Some(b'\r'),
            b'v' => Some(0x0b),
            b'f' => Some(0x0c),
            b'\\' => Some(b'\\'),
            b'"' => Some(b'"'),
            _ => None,
        };
        if let Some(byte) = named {
            out.push(byte);
            at += 2;
            continue;
        }
        let digits = bytes[at + 1..]
            .iter()
            .take(3)
            .take_while(|byte| (b'0'..=b'7').contains(byte))
            .count();
        if digits == 0 {
            out.push(b'\\');
            at += 1;
            continue;
        }
        let value = bytes[at + 1..at + 1 + digits]
            .iter()
            .fold(0u32, |value, digit| value * 8 + u32::from(digit - b'0'));
        out.push(value as u8);
        at += 1 + digits;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The first argument of a `getdents64`: the descriptor, and the path strace printed for it
/// with `-y`. Returns the path and the rest of the line.
fn descriptor(args: &str) -> Result<(Option<String>, &str)> {
    let number = args
        .bytes()
        .take_while(|byte| byte.is_ascii_digit() || *byte == b'-')
        .count();
    ensure!(number > 0, "no descriptor");
    let after = &args[number..];
    let Some(named) = after.strip_prefix('<') else {
        return Ok((None, after));
    };
    // strace escapes '<' and '>' inside the path, so the first '>' ends it.
    let (path, rest) = named
        .split_once('>')
        .context("no end of the descriptor's path")?;
    Ok((
        Some(unescape(path)),
        rest.strip_prefix("(deleted)").unwrap_or(rest),
    ))
}

/// The first argument of an `execve`: the program, and the rest of the line.
fn program(args: &str) -> (String, &str) {
    let Some(quoted) = args.strip_prefix('"') else {
        // A pointer strace could not read.
        return (String::from("?"), args);
    };
    let bytes = quoted.as_bytes();
    let mut at = 0;
    while at < bytes.len() && bytes[at] != b'"' {
        at += if bytes[at] == b'\\' { 2 } else { 1 };
    }
    let end = at.min(bytes.len());
    (unescape(&quoted[..end]), &quoted[end..])
}

/// The result strace printed after a call's arguments, or `None` for a call that is still
/// running at the end of this line.
fn result_of(rest: &str) -> Result<Option<&str>> {
    let rest = rest.trim_end();
    if rest.ends_with(UNFINISHED) {
        return Ok(None);
    }
    let (_, result) = rest.rsplit_once(" = ").context("a call with no result")?;
    Ok(Some(result.trim()))
}

/// A call's arguments as strace printed them, split at the commas between them (not those
/// in a quoted string or in a descriptor's `<path>`), up to the closing parenthesis or the
/// end of an unfinished call; and what follows them.
fn call_args(text: &str) -> (Vec<String>, &str) {
    let bytes = text.as_bytes();
    let (mut args, mut start, mut at) = (Vec::new(), 0, 0);
    let (mut quoted, mut angle) = (false, false);
    while at < bytes.len() {
        let byte = bytes[at];
        if quoted {
            if byte == b'\\' {
                at += 2;
                continue;
            }
            quoted = byte != b'"';
        } else if angle {
            angle = byte != b'>';
        } else if byte == b'"' {
            quoted = true;
        } else if byte == b'<' {
            if text[at..].starts_with(UNFINISHED) {
                break;
            }
            angle = true;
        } else if byte == b',' {
            args.push(text[start..at].trim().to_string());
            start = at + 1;
        } else if byte == b')' {
            break;
        }
        at += 1;
    }
    let last = text[start..at.min(text.len())].trim();
    if !last.is_empty() {
        args.push(last.to_string());
    }
    (args, &text[at.min(text.len())..])
}

/// The string a quoted argument holds.
fn string_arg(arg: &str) -> Option<String> {
    let inner = arg.strip_prefix('"')?.strip_suffix('"')?;
    Some(unescape(inner))
}

/// The path `-y` printed for a directory descriptor argument (`AT_FDCWD</cwd>`, `5</dir>`).
fn dir_arg(arg: &str) -> Option<String> {
    let (_, named) = arg.split_once('<')?;
    let path = named.strip_suffix('>')?;
    Some(unescape(path))
}

/// The directory the entry `path`, relative to the directory descriptor `dir` when it is not
/// absolute, is created in.
fn place(dir: Option<&str>, path: &str) -> Placed {
    let full = if path.starts_with('/') {
        PathBuf::from(path)
    } else {
        match dir.and_then(dir_arg) {
            Some(dir) => Path::new(&dir).join(path),
            None => return Placed::Unplaced,
        }
    };
    // `a/b/` names the entry `b`: its directory is `a`, like `a/b`'s.
    let full: PathBuf = full.components().collect();
    if full
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(makes_a_bucket)
    {
        return Placed::Bucket;
    }
    match full.parent() {
        Some(parent) => Placed::In(parent.display().to_string()),
        None => Placed::Unplaced,
    }
}

/// The calls that can create a directory entry, by name, with where each puts it: the index
/// of its directory descriptor argument (if it takes one) and of the entry's path.
const CREATING: [(&str, Option<usize>, usize); 15] = [
    ("openat", Some(0), 1),
    ("open", None, 0),
    ("creat", None, 0),
    ("mkdirat", Some(0), 1),
    ("mkdir", None, 0),
    ("renameat2", Some(2), 3),
    ("renameat", Some(2), 3),
    ("rename", None, 1),
    ("linkat", Some(2), 3),
    ("link", None, 1),
    ("symlinkat", Some(1), 2),
    ("symlink", None, 1),
    ("mknodat", Some(0), 1),
    ("mknod", None, 0),
    ("openat2", Some(0), 1),
];

/// Where the call `name` with arguments `args` creates an entry, or `None` for a call that
/// creates none (an open without O_CREAT).
fn placed_by(name: &str, args: &[String]) -> Result<Option<Placed>> {
    let &(_, dir, path) = CREATING
        .iter()
        .find(|(call, _, _)| *call == name)
        .context("not an entry-creating call")?;
    let flags = match name {
        "openat" | "openat2" => args.get(2),
        "open" => args.get(1),
        _ => None,
    };
    if matches!(name, "openat" | "openat2" | "open")
        && !flags.is_some_and(|flags| flags.contains("O_CREAT"))
    {
        return Ok(None);
    }
    let path = args
        .get(path)
        .and_then(|arg| string_arg(arg))
        .with_context(|| format!("{name} printed no path: {args:?}"))?;
    Ok(Some(place(
        dir.and_then(|dir| args.get(dir)).map(String::as_str),
        &path,
    )))
}

fn finish_create(placed: Option<Placed>, result: &str, trace: &mut Trace) {
    // "?" is a process that died in the call. A call that failed ("-1 EEXIST", a rename that
    // lost, an O_CREAT open of a file another start made) still took the directory's lock to
    // find that out, so it counts like one that made its entry.
    if result.starts_with('?') {
        return;
    }
    match placed {
        Some(Placed::In(dir)) => *trace.creates.entry(dir).or_default() += 1,
        Some(Placed::Unplaced) => trace.unplaced_creates += 1,
        Some(Placed::Bucket) | None => {}
    }
}

fn finish_listing(
    reader: String,
    dir: Option<String>,
    rest: &str,
    result: &str,
    trace: &mut Trace,
) -> Result<()> {
    // "?" is a process that died in the call, "-1 ..." a call that failed: nothing was read.
    if result.starts_with('?') || result.starts_with('-') {
        return Ok(());
    }
    let bytes: u64 = result
        .split_whitespace()
        .next()
        .context("an empty result")?
        .parse()
        .with_context(|| format!("the result {result:?} is not a byte count"))?;
    let entries: u64 = match rest.split_once("/* ") {
        Some((_, comment)) => {
            let (count, _) = comment
                .split_once(" entr")
                .with_context(|| format!("the comment {comment:?} is not an entry count"))?;
            count
                .parse()
                .with_context(|| format!("the entry count {count:?} is not a number"))?
        }
        None => {
            ensure!(
                bytes == 0,
                "strace printed no entry count for a read of {bytes} bytes"
            );
            0
        }
    };
    match dir {
        Some(dir) => {
            let read = trace.dirs.entry(dir).or_default();
            read.entries += entries;
            read.readers.insert(reader);
        }
        None => trace.unnamed_entries += entries,
    }
    Ok(())
}

fn parse_line(line: &str, trace: &mut Trace, processes: &mut Processes) -> Result<()> {
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    ensure!(digits > 0, "no process id");
    let pid: u32 = line[..digits].parse().context("the process id")?;
    let after = &line[digits..];
    // `-Y` prints the command name after the id.
    let (comm, rest) = match after.strip_prefix('<') {
        Some(named) => named
            .split_once("> ")
            .context("no end of the command name")?,
        None => ("", after),
    };
    let rest = rest.trim_start();

    if let Some(note) = rest.strip_prefix("+++ ") {
        // A thread that execs takes over its process's id, and the call ends under that id.
        match note.strip_prefix("superseded by execve in pid ") {
            Some(by) => {
                let thread: u32 = by
                    .split_whitespace()
                    .next()
                    .context("no thread id")?
                    .parse()
                    .context("the thread id")?;
                if let Some(call) = processes.pending.remove(&thread) {
                    processes.pending.insert(pid, call);
                }
            }
            // Exited or killed.
            None => {
                processes.pending.remove(&pid);
                processes.program.remove(&pid);
            }
        }
        return Ok(());
    }
    // A signal.
    if rest.starts_with("--- ") {
        return Ok(());
    }

    for call in ["getdents64", "getdents"] {
        if let Some(args) = rest
            .strip_prefix(call)
            .and_then(|rest| rest.strip_prefix('('))
        {
            let (dir, rest) = descriptor(args)?;
            return match result_of(rest)? {
                Some(result) => {
                    finish_listing(processes.reader(pid, comm), dir, rest, result, trace)
                }
                None => {
                    processes.pending.insert(pid, Pending::Listing(dir));
                    Ok(())
                }
            };
        }
        if let Some(rest) = rest.strip_prefix(&format!("<... {call} resumed>")) {
            let Some(Pending::Listing(dir)) = processes.pending.remove(&pid) else {
                bail!("the end of a {call} whose beginning is not in the log");
            };
            let result = result_of(rest)?.context("a resumed call that is unfinished again")?;
            return finish_listing(processes.reader(pid, comm), dir, rest, result, trace);
        }
    }

    if let Some(args) = rest.strip_prefix("execve(") {
        let (program, rest) = program(args);
        match result_of(rest)? {
            Some("0") => processes.became(pid, program, trace),
            Some(_) => {}
            None => {
                processes.pending.insert(pid, Pending::Exec(program));
            }
        }
        return Ok(());
    }
    if let Some(rest) = rest.strip_prefix("<... execve resumed>") {
        let Some(Pending::Exec(program)) = processes.pending.remove(&pid) else {
            bail!("the end of an execve whose beginning is not in the log");
        };
        if result_of(rest)? == Some("0") {
            processes.became(pid, program, trace);
        }
        return Ok(());
    }
    for (call, _, _) in CREATING {
        if let Some(args) = rest
            .strip_prefix(call)
            .and_then(|rest| rest.strip_prefix('('))
        {
            let (args, after) = call_args(args);
            let placed = placed_by(call, &args)?;
            match result_of(after)? {
                Some(result) => finish_create(placed, result, trace),
                None => {
                    processes.pending.insert(pid, Pending::Create(placed));
                }
            }
            return Ok(());
        }
        if let Some(rest) = rest.strip_prefix(&format!("<... {call} resumed>")) {
            let Some(Pending::Create(placed)) = processes.pending.remove(&pid) else {
                bail!("the end of a {call} whose beginning is not in the log");
            };
            let result = result_of(rest)?.context("a resumed call that is unfinished again")?;
            finish_create(placed, result, trace);
            return Ok(());
        }
    }
    bail!("not a line of the calls this test asks strace for")
}

/// Read strace's log of [`TRACED`] calls. The log may still be growing: a last line without
/// its newline is not read. Any other line this does not understand is an error, because a
/// line skipped could be a listing.
fn parse_trace(log: &str) -> Result<Trace> {
    let complete = log.rfind('\n').map_or("", |end| &log[..end]);
    let mut trace = Trace::default();
    let mut processes = Processes::default();
    for (index, line) in complete.lines().enumerate() {
        parse_line(line, &mut trace, &mut processes)
            .with_context(|| format!("strace log line {}: {line}", index + 1))?;
    }
    Ok(trace)
}

/// A directory the test names: a path under `path` is compared as `name` and what follows.
struct Root {
    name: &'static str,
    path: PathBuf,
}

/// The name a directory is compared under in both starts (see the module doc).
fn dir_key(dir: &str, roots: &[Root]) -> String {
    let under = roots
        .iter()
        .filter_map(|root| {
            let rest = Path::new(dir).strip_prefix(&root.path).ok()?;
            Some((root, rest))
        })
        .max_by_key(|(root, _)| root.path.as_os_str().len());
    if let Some((root, rest)) = under {
        return if rest.as_os_str().is_empty() {
            root.name.to_string()
        } else {
            format!("{}/{}", root.name, rest.display())
        };
    }
    let Some(rest) = dir.strip_prefix("/proc/") else {
        return dir.to_string();
    };
    let parts: Vec<&str> = rest
        .split('/')
        .map(|part| {
            if !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()) {
                "<pid>"
            } else {
                part
            }
        })
        .collect();
    format!("/proc/{}", parts.join("/"))
}

/// The entries one start read, per directory name ([`dir_key`]).
type Reads = BTreeMap<String, DirRead>;

fn reads_of(trace: &Trace, roots: &[Root]) -> Reads {
    let mut reads = Reads::new();
    for (dir, read) in &trace.dirs {
        let merged = reads.entry(dir_key(dir, roots)).or_default();
        merged.entries += read.entries;
        merged.readers.extend(read.readers.iter().cloned());
    }
    reads
}

/// A directory a start is known to list once per VM alive, with the issue that tracks it.
/// The entry is checked both ways: the listing is not reported, and an entry whose
/// directory no longer grows fails the test until the entry is removed.
///
/// Only a directory the test's own starts own can be allowed: one under its data directory
/// or its scopes' parent ([`owned_by_the_test`]). The entries of a host-wide directory
/// (`/run/netns`, `/proc`) also follow VMs other tests start and stop, so neither "it
/// grows" nor "it no longer grows" could be decided for it. A start that lists one is
/// fixed before this test lands on its tree (#2194 was).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Allowance {
    dir: &'static str,
    issue: u32,
}

/// Whether `dir`, as [`dir_key`] names it, is under a directory the test made for itself.
fn owned_by_the_test(dir: &str) -> bool {
    ["<data>", "<scopes>"].iter().any(|root| {
        dir.strip_prefix(root)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Finding {
    /// The second start read at least one entry more from `dir` for each VM added.
    Grows {
        dir: String,
        beside_few: u64,
        beside_many: u64,
    },
    /// An allowance names a directory the test does not own.
    ForeignAllowance { dir: &'static str, issue: u32 },
    /// An allowed directory did not grow: its issue is fixed and its entry must go.
    StaleAllowance {
        dir: &'static str,
        issue: u32,
        beside_few: u64,
        beside_many: u64,
    },
}

/// The name the directories that only one of the two starts listed are compared under.
const LISTED_BY_ONE_START: &str = "<directories only one of the two starts listed>";

/// Compare what a start read beside few VMs with what a start read beside `added` more.
fn judge(few: &Reads, many: &Reads, added: u64, allowances: &[Allowance]) -> Vec<Finding> {
    let entries = |reads: &Reads, dir: &str| reads.get(dir).map_or(0, |read| read.entries);
    let grows = |beside_few: u64, beside_many: u64| beside_many >= beside_few + added;
    let mut findings = Vec::new();
    for allowance in allowances {
        if !owned_by_the_test(allowance.dir) {
            findings.push(Finding::ForeignAllowance {
                dir: allowance.dir,
                issue: allowance.issue,
            });
            continue;
        }
        let (beside_few, beside_many) = (entries(few, allowance.dir), entries(many, allowance.dir));
        if !grows(beside_few, beside_many) {
            findings.push(Finding::StaleAllowance {
                dir: allowance.dir,
                issue: allowance.issue,
                beside_few,
                beside_many,
            });
        }
    }
    let (mut only_few, mut only_many) = (0, 0);
    let dirs: BTreeSet<&String> = few.keys().chain(many.keys()).collect();
    for dir in dirs {
        if allowances
            .iter()
            .any(|allowance| allowance.dir == dir && owned_by_the_test(allowance.dir))
        {
            continue;
        }
        match (few.get(dir), many.get(dir)) {
            (Some(beside_few), Some(beside_many)) => {
                if grows(beside_few.entries, beside_many.entries) {
                    findings.push(Finding::Grows {
                        dir: dir.clone(),
                        beside_few: beside_few.entries,
                        beside_many: beside_many.entries,
                    });
                }
            }
            (Some(read), None) => only_few += read.entries,
            (None, Some(read)) => only_many += read.entries,
            (None, None) => {}
        }
    }
    if grows(only_few, only_many) {
        findings.push(Finding::Grows {
            dir: LISTED_BY_ONE_START.to_string(),
            beside_few: only_few,
            beside_many: only_many,
        });
    }
    findings
}

/// The entry-creating calls of one start, per directory name ([`create_key`]).
type Creates = BTreeMap<String, u64>;

/// The name a directory a start created an entry in is compared under: [`dir_key`]'s, with
/// every component that is a VM's id (`vm-` and 32 hex digits) as `<vm>`, so a directory of
/// the start's own VM is named the same in every start and is known to be its own, and every
/// component that is a bucket of a hashed layout (one or two lowercase hex digits) as
/// `<b16>` or `<b256>`, the number of buckets a name of that length is one of, so the
/// buckets of one layout are one family whatever bucket each start's key fell in.
fn create_key(dir: &str, roots: &[Root]) -> String {
    dir_key(dir, roots)
        .split('/')
        .map(|part| {
            let id = part.strip_prefix("vm-");
            if id.is_some_and(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())) {
                "<vm>".to_string()
            } else if let Some(buckets) = bucket_family(part) {
                format!("<b{buckets}>")
            } else if part
                .strip_prefix("boot-")
                .is_some_and(|boot| boot.len() >= 8 && boot.bytes().all(|b| b.is_ascii_hexdigit()))
            {
                // A runtime directory's boot: one name in every start of a run.
                "boot-<id>".to_string()
            } else {
                part.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// How many buckets a directory named `part` is one of, when it is a bucket of a hashed
/// layout: 16 for one lowercase hex digit, 256 for two.
fn bucket_family(part: &str) -> Option<u32> {
    let hex = part
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    match part.len() {
        1 | 2 if hex => Some(16u32.pow(part.len() as u32)),
        _ => None,
    }
}

/// The buckets a directory key's family spreads starts over: the product of its bucket
/// components' families, 1 when it has none.
fn fan_out(key: &str) -> u32 {
    key.split('/')
        .filter_map(|part| {
            part.strip_prefix("<b")?
                .strip_suffix('>')?
                .parse::<u32>()
                .ok()
        })
        .product()
}

/// The fewest buckets a family every start writes into may have: with more starts than
/// that at once, each bucket's lock is shared by a sixty-fourth of them at most.
const MIN_FAN_OUT: u32 = 64;

fn creates_of(trace: &Trace, roots: &[Root]) -> Creates {
    let mut creates = Creates::new();
    for (dir, calls) in &trace.creates {
        *creates.entry(create_key(dir, roots)).or_default() += calls;
    }
    creates
}

/// A store directory more than one traced start creates an entry in, or a family of buckets
/// every start writes into that has fewer than [`MIN_FAN_OUT`] buckets, with the calls of
/// each start and the family's buckets (1 for a plain directory).
#[derive(Debug, Clone, PartialEq, Eq)]
struct SharedCreate {
    dir: String,
    per_start: Vec<u64>,
    fan_out: u32,
}

/// A store directory every start is still known to create an entry in, with its issue.
/// Checked both ways, like [`Allowance`]: an entry whose directory some start no longer
/// writes into fails until it is removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CreateAllowance {
    dir: &'static str,
    issue: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CreateFinding {
    Shared(SharedCreate),
    StaleAllowance { dir: &'static str, issue: u32 },
}

/// The directories under the data directory, no start's own (no `<vm>` component), that the
/// starts share. Each entry-creating call takes the directory's inode lock for writing, and on
/// btrfs the holder can wait out a transaction commit, so the starts that write into one
/// directory take turns, and the rate is bounded by how many starts each directory is
/// shared by. What bounds it is judged from the layout, not from which bucket six random
/// keys fell in: a plain directory that two or more starts write into is shared by all of
/// them; a family of buckets every start writes into shares each bucket among one start in
/// its fan-out, which must be at least [`MIN_FAN_OUT`].
fn judge_creates(starts: &[Creates], allowances: &[CreateAllowance]) -> Vec<CreateFinding> {
    let writers = |dir: &str| {
        starts
            .iter()
            .filter(|start| start.get(dir).is_some_and(|n| *n > 0))
            .count()
    };
    let shared = |dir: &str| {
        let fan_out = fan_out(dir);
        if fan_out == 1 {
            writers(dir) >= 2
        } else {
            writers(dir) == starts.len() && fan_out < MIN_FAN_OUT
        }
    };
    let mut findings = Vec::new();
    for allowance in allowances {
        if !shared(allowance.dir) {
            findings.push(CreateFinding::StaleAllowance {
                dir: allowance.dir,
                issue: allowance.issue,
            });
        }
    }
    let dirs: BTreeSet<&String> = starts.iter().flat_map(|start| start.keys()).collect();
    for dir in dirs {
        let ours = dir == "<data>" || dir.starts_with("<data>/");
        let a_vms_own = dir.split('/').any(|part| part == "<vm>");
        if !ours || a_vms_own || !shared(dir) {
            continue;
        }
        if allowances.iter().any(|allowance| allowance.dir == dir) {
            continue;
        }
        findings.push(CreateFinding::Shared(SharedCreate {
            dir: dir.clone(),
            per_start: starts
                .iter()
                .map(|start| start.get(dir).copied().unwrap_or(0))
                .collect(),
            fan_out: fan_out(dir),
        }));
    }
    findings
}

/// Every directory any start created an entry in, with each start's calls.
fn create_table(starts: &[Creates]) -> String {
    let dirs: BTreeSet<&String> = starts.iter().flat_map(|start| start.keys()).collect();
    let mut text = String::from("  calls per start  directory\n");
    for dir in dirs {
        let calls: Vec<String> = starts
            .iter()
            .map(|start| {
                start
                    .get(dir)
                    .map_or_else(|| "-".to_string(), u64::to_string)
            })
            .collect();
        text.push_str(&format!("  {:>15}  {dir}\n", calls.join(" ")));
    }
    text
}

/// Every directory either start listed, with the entries each read and who read them.
fn table(few: &Reads, many: &Reads) -> String {
    let dirs: BTreeSet<&String> = few.keys().chain(many.keys()).collect();
    let mut text = String::from("  beside few  beside many  directory (read by)\n");
    for dir in dirs {
        let count = |reads: &Reads| {
            reads
                .get(dir)
                .map_or_else(|| "-".to_string(), |read| read.entries.to_string())
        };
        let readers: BTreeSet<&String> = [few, many]
            .iter()
            .filter_map(|reads| reads.get(dir))
            .flat_map(|read| read.readers.iter())
            .collect();
        let readers: Vec<&str> = readers.into_iter().map(String::as_str).collect();
        text.push_str(&format!(
            "  {:>10}  {:>11}  {dir} ({})\n",
            count(few),
            count(many),
            readers.join(", ")
        ));
    }
    text
}

/// The calls strace logs: every program a process becomes, every directory read, and every
/// call that can create a directory entry ([`CREATING`]). A `?` keeps strace from refusing a
/// call the architecture does not have (arm64 has only the `*at` forms and `renameat2`).
const TRACED: &str =
    "trace=execve,?getdents,getdents64,openat,?openat2,?open,?creat,mkdirat,?mkdir,\
     renameat2,?renameat,?rename,linkat,?link,symlinkat,?symlink,mknodat,?mknod";

/// strace's arguments before the command it runs. `-D` makes strace the grandchild and
/// the command the child, so the caller's child is the command itself, with its pid. `-f`
/// follows every process and thread the command creates, `-y` prints each descriptor's
/// path, `-Y` each process's command name, and `-s` keeps a program's path whole.
fn tracer_args(log: &Path) -> Vec<String> {
    let mut args: Vec<String> = ["-D", "-f", "-y", "-Y", "-s", "4096", "-e", TRACED, "-o"]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
    args.push(log.display().to_string());
    args
}

/// Run `command` to its end under strace and read the log.
fn trace_of(command: &[&str]) -> Result<Trace> {
    let dir = tempfile::tempdir()?;
    let log = dir.path().join("trace");
    // The tracer holds the stderr pipe until it has written its last line, so reading the
    // pipe to its end waits for the whole log.
    let output = match std::process::Command::new("strace")
        .args(tracer_args(&log))
        .args(command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => bail!(
            "strace is not installed, so this test cannot see which directories a start \
             lists and fails. Install it (apt-get install strace)."
        ),
        Err(error) => return Err(error).context("running strace"),
    };
    ensure!(
        output.status.success(),
        "strace {} {command:?} exited with {}: {}. This test needs a strace that takes those \
         options and may trace its own children.",
        tracer_args(&log).join(" "),
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let text = std::fs::read(&log).with_context(|| format!("reading {}", log.display()))?;
    parse_trace(&String::from_utf8_lossy(&text))
}

/// Fail unless strace can do what this file needs of it: run a command, follow it and log
/// the program it became. Checked once per test process.
fn require_tracer() -> Result<()> {
    static CHECKED: std::sync::OnceLock<std::result::Result<(), String>> =
        std::sync::OnceLock::new();
    CHECKED
        .get_or_init(|| {
            let trace = trace_of(&["true"]).map_err(|error| format!("{error:#}"))?;
            match trace.execs.first() {
                Some((_, program)) if program.ends_with("/true") => Ok(()),
                other => Err(format!(
                    "strace ran `true` and logged {other:?} as the first program: this test \
                     cannot rely on its log"
                )),
            }
        })
        .clone()
        .map_err(anyhow::Error::msg)
}

mod parsing {
    use super::*;

    fn read(entries: u64, readers: &[&str]) -> DirRead {
        DirRead {
            entries,
            readers: readers.iter().map(|reader| reader.to_string()).collect(),
        }
    }

    // The fixtures are strace 6.8's output with made-up paths and process ids.

    #[test]
    fn a_listing_is_summed_over_its_calls_and_named_by_its_path() {
        let trace = parse_trace(concat!(
            "4101<ls> getdents64(3</srv/store/big>, 0xb2f0912421a0 /* 1365 entries */, 32768) = 32760\n",
            "4101<ls> getdents64(3</srv/store/big>, 0xb2f0912421a0 /* 272 entries */, 32768) = 6528\n",
            "4101<ls> getdents64(3</srv/store/big>, 0xb2f0912421a0 /* 0 entries */, 32768) = 0\n",
            "4101<ls> getdents64(4</srv/store/one>, 0xf65dce5b00f0 /* 1 entries */, 40) = 32\n",
            "4101<ls> +++ exited with 0 +++\n",
        ))
        .unwrap();
        assert_eq!(
            trace.dirs,
            BTreeMap::from([
                ("/srv/store/big".to_string(), read(1637, &["ls"])),
                ("/srv/store/one".to_string(), read(1, &["ls"])),
            ])
        );
        assert_eq!(trace.unnamed_entries, 0);
    }

    #[test]
    fn an_entry_creating_call_is_counted_in_the_directory_it_creates_in() {
        let trace = parse_trace(concat!(
            "5101<fcvm> mkdirat(AT_FDCWD</home/u>, \"/srv/store/vm-disks/vm-1\", 0777) = 0\n",
            "5101<fcvm> openat(AT_FDCWD</home/u>, \"/srv/store/state/vms/a/vm-1.json.tmp\", O_WRONLY|O_CREAT|O_TRUNC|O_CLOEXEC, 0666) = 9</srv/store/state/vms/a/vm-1.json.tmp>\n",
            "5101<fcvm> openat(AT_FDCWD</home/u>, \"/srv/store/state/vms/a/vm-2.json\", O_RDONLY|O_CLOEXEC) = 9</srv/store/state/vms/a/vm-2.json>\n",
            "5101<fcvm> renameat2(AT_FDCWD</home/u>, \"/srv/store/state/vms/a/vm-1.json.tmp\", AT_FDCWD</home/u>, \"/srv/store/state/vms/a/vm-1.json\", 0) = 0\n",
            "5101<fcvm> openat(7</srv/store/snap>, \"slot-0.lock\", O_RDWR|O_CREAT|O_CLOEXEC, 0644) = 8</srv/store/snap/slot-0.lock>\n",
            "5101<fcvm> mkdirat(AT_FDCWD</srv/store>, \"vm-disks/vm-1/disks/\", 0777) = 0\n",
            "5101<fcvm> symlinkat(\"/a\", AT_FDCWD</home/u>, \"/srv/store/links/l\") = 0\n",
            "5101<fcvm> linkat(AT_FDCWD</home/u>, \"/srv/a\", AT_FDCWD</home/u>, \"/srv/store/links/h\", 0) = 0\n",
            "5101<fcvm> mknodat(AT_FDCWD</home/u>, \"/srv/store/links/n\", S_IFIFO|0600) = 0\n",
        ))
        .unwrap();
        assert_eq!(
            trace.creates,
            BTreeMap::from([
                ("/srv/store/links".to_string(), 3),
                ("/srv/store/snap".to_string(), 1),
                ("/srv/store/state/vms/a".to_string(), 2),
                ("/srv/store/vm-disks".to_string(), 1),
                ("/srv/store/vm-disks/vm-1".to_string(), 1),
            ])
        );
        assert_eq!(trace.unplaced_creates, 0);
    }

    /// A create that failed (a mkdir of a directory that exists) took the directory's lock
    /// as one that succeeded does, so it counts; one whose process died in it does not.
    #[test]
    fn a_failed_create_counts_one_that_never_ended_does_not_and_a_split_one_counts_once() {
        let trace = parse_trace(concat!(
            "5111<fcvm> mkdirat(AT_FDCWD</h>, \"/srv/store/vm-disks\", 0777) = -1 EEXIST (File exists)\n",
            "5111<fcvm> openat(AT_FDCWD</h>, \"/srv/store/x.lock\", O_RDWR|O_CREAT|O_CLOEXEC, 0666 <unfinished ...>\n",
            "5112<fcvm> openat(AT_FDCWD</h>, \"/srv/store/y\", O_RDONLY <unfinished ...>\n",
            "5111<fcvm> <... openat resumed>) = 5</srv/store/x.lock>\n",
            "5112<fcvm> <... openat resumed>) = -1 ENOENT (No such file or directory)\n",
            "5113<fcvm> mkdirat(AT_FDCWD</h>, \"/srv/store/z\", 0777 <unfinished ...>\n",
            "5113<fcvm> +++ killed by SIGKILL +++\n",
        ))
        .unwrap();
        assert_eq!(
            trace.creates,
            BTreeMap::from([("/srv/store".to_string(), 2)])
        );
    }

    #[test]
    fn making_a_bucket_is_not_counted_as_a_create() {
        let trace = parse_trace(concat!(
            "5131<fcvm> mkdirat(AT_FDCWD</h>, \"/srv/store/vm-disks/.7f.0123456789abcdef0123456789abcdef\", 0777) = 0\n",
            "5131<fcvm> renameat2(AT_FDCWD</h>, \"/srv/store/vm-disks/.7f.0123456789abcdef0123456789abcdef\", AT_FDCWD</h>, \"/srv/store/vm-disks/7f\", RENAME_NOREPLACE) = 0\n",
            "5131<fcvm> mkdirat(AT_FDCWD</h>, \"/srv/store/state/names/.index-staging.4.41d49fbb-6143-4a3d-8cc3-6a8568fb768b\", 0777) = 0\n",
            "5131<fcvm> mkdirat(AT_FDCWD</h>, \"/srv/store/vm-disks/7f/vm-1\", 0777) = 0\n",
            "5131<fcvm> openat(AT_FDCWD</h>, \"/srv/store/snapshots/s.lock\", O_WRONLY|O_CREAT|O_CLOEXEC, 0666) = 9</srv/store/snapshots/s.lock>\n",
        ))
        .unwrap();
        assert_eq!(
            trace.creates,
            BTreeMap::from([
                ("/srv/store/snapshots".to_string(), 1),
                ("/srv/store/vm-disks/7f".to_string(), 1),
            ])
        );
        for name in ["a", "7f", ".7f.0123456789abcdef0123456789abcdef"] {
            assert!(makes_a_bucket(name), "{name}");
        }
        for name in ["vm-1", "abc", "s.lock", ".lock", "7F", ".x.y"] {
            assert!(!makes_a_bucket(name), "{name}");
        }
    }

    #[test]
    fn a_relative_create_with_no_directory_descriptor_is_counted_as_unplaced() {
        let trace = parse_trace(concat!(
            "5121<sh> openat(AT_FDCWD</srv/store>, \"made\", O_WRONLY|O_CREAT|O_TRUNC, 0666) = 3</srv/store/made>\n",
            "5121<sh> mkdir(\"rel\", 0777) = 0\n",
            "5121<sh> mkdir(\"/srv/abs\", 0777) = 0\n",
        ))
        .unwrap();
        assert_eq!(
            trace.creates,
            BTreeMap::from([("/srv".to_string(), 1), ("/srv/store".to_string(), 1)])
        );
        assert_eq!(trace.unplaced_creates, 1);
    }

    #[test]
    fn a_path_keeps_its_spaces_and_its_escaped_bytes() {
        let trace = parse_trace(concat!(
            "4102<ls> getdents64(3</srv/store/sp ace>, 0xb505bdacd2e0 /* 2 entries */, 32768) = 48\n",
            "4102<ls> getdents64(3</srv/store/an\\76gle>, 0xb505bdacd2e0 /* 3 entries */, 32768) = 72\n",
            "4102<ls> getdents64(4</srv/store/gone>(deleted), 0xb505bdacd2e0 /* 2 entries */, 32768) = 48\n",
        ))
        .unwrap();
        let dirs: Vec<&str> = trace.dirs.keys().map(String::as_str).collect();
        assert_eq!(
            dirs,
            ["/srv/store/an>gle", "/srv/store/gone", "/srv/store/sp ace"]
        );
    }

    #[test]
    fn a_call_split_by_another_thread_is_counted_once_it_ends() {
        let trace = parse_trace(concat!(
            "4111<python3> getdents64(3</srv/store/big>,  <unfinished ...>\n",
            "4112<python3> getdents64(7</proc>,  <unfinished ...>\n",
            "4111<python3> <... getdents64 resumed>0xf0a080000bd0 /* 1365 entries */, 32768) = 32760\n",
            "4112<python3> <... getdents64 resumed>0xf0a064000bd0 /* 1290 entries */, 32768) = 32744\n",
            // Still in the call when the log was read.
            "4111<python3> getdents64(3</srv/store/big>,  <unfinished ...>\n",
        ))
        .unwrap();
        assert_eq!(trace.dirs["/srv/store/big"], read(1365, &["python3"]));
        assert_eq!(trace.dirs["/proc"], read(1290, &["python3"]));
    }

    #[test]
    fn a_failed_call_and_a_process_that_died_in_one_read_nothing() {
        let trace = parse_trace(concat!(
            "4121<python3> getdents64(3</etc/hostname>, 0x16040210, 1024) = -1 ENOTDIR (Not a directory)\n",
            "4121<python3> getdents64(5<pipe:[1638372]>, 0x16040210, 1024) = -1 ENOTDIR (Not a directory)\n",
            "4121<python3> getdents64(999, 0x16040210, 1024) = -1 EBADF (Bad file descriptor)\n",
            "4121<python3> getdents64(3</srv/store/d>, 0xf65dce5b00f0, 10) = -1 EINVAL (Invalid argument)\n",
            "4122<python3> getdents64(3</proc>,  <unfinished ...>\n",
            "4122<python3> <... getdents64 resumed> <unfinished ...>) = ?\n",
            "4122<python3> +++ killed by SIGKILL +++\n",
        ))
        .unwrap();
        assert_eq!(trace, Trace::default());
    }

    #[test]
    fn entries_read_through_a_descriptor_with_no_path_are_kept_apart() {
        let trace =
            parse_trace("4131<fcvm> getdents64(9, 0xb505bdacd2e0 /* 7 entries */, 32768) = 168\n")
                .unwrap();
        assert!(trace.dirs.is_empty());
        assert_eq!(trace.unnamed_entries, 7);
    }

    #[test]
    fn each_program_a_process_became_is_recorded_and_a_failed_exec_is_not() {
        let trace = parse_trace(concat!(
            "4141<strace> execve(\"/usr/bin/sh\", [\"sh\", \"-c\", \"ls d; echo a = b\"], 0xffffe7050040 /* 56 vars */) = 0\n",
            "4142<sh> execve(\"/usr/bin/env\", [\"env\", \"-i\", \"ls\", \"d\"], 0xc92bc1362188 /* 56 vars */) = 0\n",
            "4142<env> execve(\"/nonexistent/ls\", [\"ls\", \"d\"], 0xc02b4bd0f1c0 /* 1 var */) = -1 ENOENT (No such file or directory)\n",
            "4142<env> execve(\"/usr/bin/ls\", [\"ls\", \"d\"], 0xc02b4bd0f1c0 /* 1 var */) = 0\n",
            "4142<ls> getdents64(3</srv/store/d>, 0xc265877c3640 /* 8 entries */, 32768) = 208\n",
            "4142<ls> +++ exited with 0 +++\n",
            "4141<sh> --- SIGCHLD {si_signo=SIGCHLD, si_code=CLD_EXITED, si_pid=4142, si_uid=1000, si_status=0, si_utime=0, si_stime=0} ---\n",
            "4141<sh> +++ exited with 0 +++\n",
        ))
        .unwrap();
        assert_eq!(
            trace.execs,
            [
                (4141, "/usr/bin/sh".to_string()),
                (4142, "/usr/bin/env".to_string()),
                (4142, "/usr/bin/ls".to_string()),
            ]
        );
        assert_eq!(trace.dirs["/srv/store/d"], read(8, &["ls"]));
    }

    #[test]
    fn a_reader_is_named_by_the_program_it_became_not_by_a_stale_command_name() {
        // strace 6.8 printed the parent's name for an `ip` that fcvm had just started.
        let trace = parse_trace(concat!(
            "4145<strace> execve(\"/usr/bin/fcvm\", [\"fcvm\"], 0xffffe7050040 /* 56 vars */) = 0\n",
            "4146<fcvm> execve(\"/usr/local/bin/ip\", [\"ip\", \"link\", \"show\", \"veth0\"], 0xc92bc1362188 /* 56 vars */) = -1 ENOENT (No such file or directory)\n",
            "4146<fcvm> execve(\"/usr/sbin/ip\", [\"ip\", \"link\", \"show\", \"veth0\"], 0xc92bc1362188 /* 56 vars */) = 0\n",
            "4146<fcvm> getdents64(5</run/netns>, 0xae67445d2580 /* 9 entries */, 32768) = 328\n",
            "4146<fcvm> +++ exited with 0 +++\n",
            // A child that has not exec'd, a thread, and a reused process id.
            "4147<fcvm> getdents64(5</srv/store/a>, 0xae67445d2580 /* 2 entries */, 32768) = 48\n",
            "4148<tokio-rt-worker> getdents64(5</srv/store/b>, 0xae67445d2580 /* 2 entries */, 32768) = 48\n",
            "4146<sh> getdents64(5</srv/store/c>, 0xae67445d2580 /* 2 entries */, 32768) = 48\n",
        ))
        .unwrap();
        assert_eq!(trace.dirs["/run/netns"], read(9, &["ip"]));
        assert_eq!(trace.dirs["/srv/store/a"], read(2, &["fcvm"]));
        assert_eq!(trace.dirs["/srv/store/b"], read(2, &["tokio-rt-worker"]));
        assert_eq!(trace.dirs["/srv/store/c"], read(2, &["sh"]));
    }

    #[test]
    fn an_exec_by_a_thread_ends_under_its_process_id() {
        let trace = parse_trace(concat!(
            "4152<python3> getdents64(3</proc>,  <unfinished ...>\n",
            "4153<python3> execve(\"/bin/ls\", [\"ls\", \"a = b\"], 0xfffff7892270 /* 56 vars */ <unfinished ...>\n",
            "4152<python3> <... getdents64 resumed> <unfinished ...>) = ?\n",
            "4152<python3> +++ exited with 0 +++\n",
            "4151<python3> +++ superseded by execve in pid 4153 +++\n",
            "4151<ls> <... execve resumed>)       = 0\n",
        ))
        .unwrap();
        assert_eq!(trace.execs, [(4151, "/bin/ls".to_string())]);
        assert!(trace.dirs.is_empty());
    }

    #[test]
    fn a_log_that_is_still_being_written_is_read_up_to_its_last_whole_line() {
        let trace = parse_trace(concat!(
            "4161<ls> getdents64(3</srv/store/d>, 0xb505bdacd2e0 /* 8 entries */, 32768) = 208\n",
            "4161<ls> getdents64(3</srv/store/d>, 0xb505bdacd2e0 /* 5 ent",
        ))
        .unwrap();
        assert_eq!(trace.dirs["/srv/store/d"].entries, 8);
        assert_eq!(
            parse_trace("4161<ls> getdents64(3</srv").unwrap(),
            Trace::default()
        );
    }

    #[test]
    fn a_line_the_parser_does_not_know_is_an_error_not_a_skipped_listing() {
        for log in [
            // A call this test does not ask strace for (openat is one it does since the
            // entry-creating calls are traced).
            "4171<ls> fstat(3</srv/d>, {st_mode=S_IFDIR|0755, st_size=4096, ...}) = 0\n",
            "strace: Process 4171 attached\n",
            "4171<ls> getdents64(3</srv/store/d>, 0xb505bdacd2e0, 32768) = 208\n",
            "4171<ls> getdents64(3</srv/store/d>, 0xb505bdacd2e0 /* d_reclen == 0, problem here */, 32768) = 208\n",
            "4171<ls> <... getdents64 resumed>0xb505bdacd2e0 /* 8 entries */, 32768) = 208\n",
            "4171<ls> <... execve resumed>)       = 0\n",
            "4171<ls> getdents64(3</srv/store/d>, 0xb505bdacd2e0 /* 8 entries */, 32768)\n",
        ] {
            let error = parse_trace(log).unwrap_err();
            assert!(
                format!("{error:#}").contains("strace log line 1"),
                "{log:?}: {error:#}"
            );
        }
    }
}

mod judging {
    use super::*;

    fn reads(dirs: &[(&str, u64)]) -> Reads {
        dirs.iter()
            .map(|(dir, entries)| {
                (
                    dir.to_string(),
                    DirRead {
                        entries: *entries,
                        readers: BTreeSet::from(["fcvm".to_string()]),
                    },
                )
            })
            .collect()
    }

    fn roots() -> Vec<Root> {
        vec![
            Root {
                name: "<data>",
                path: PathBuf::from("/mnt/store/dl-1"),
            },
            Root {
                name: "<scopes>",
                path: PathBuf::from("/sys/fs/cgroup/fcvm-dl-1.slice"),
            },
        ]
    }

    fn creates(dirs: &[(&str, u64)]) -> Creates {
        dirs.iter().map(|(dir, n)| (dir.to_string(), *n)).collect()
    }

    #[test]
    fn a_vms_own_directory_is_named_alike_in_every_start() {
        let roots = roots();
        for (dir, key) in [
            (
                "/mnt/store/dl-1/vm-disks/vm-0123456789abcdef0123456789abcdef/disks",
                "<data>/vm-disks/<vm>/disks",
            ),
            ("/mnt/store/dl-1/vm-disks", "<data>/vm-disks"),
            ("/mnt/store/dl-1/vm-disks/vm-12", "<data>/vm-disks/vm-12"),
            (
                "/mnt/store/dl-1/vm-disks/7f/vm-0123456789abcdef0123456789abcdef",
                "<data>/vm-disks/<b256>/<vm>",
            ),
            (
                "/mnt/store/dl-1/reclaim/1000/c",
                "<data>/reclaim/1000/<b16>",
            ),
        ] {
            assert_eq!(create_key(dir, &roots), key, "{dir}");
        }
    }

    /// A plain store directory two starts write into is shared, whichever starts; a family of
    /// 16 buckets every start writes into is too narrow; a family of 256 is not, and neither
    /// is a VM's own directory, a directory one start writes into, or one outside the store.
    #[test]
    fn a_shared_store_directory_and_a_narrow_bucket_family_are_found_and_no_other() {
        let starts = vec![
            creates(&[
                ("<data>/vm-disks", 1),
                ("<data>/vm-disks/<b256>/<vm>", 2),
                ("<data>/reclaim/1000/<b16>", 3),
                ("<data>/vm-disks/<b256>", 1),
                ("<data>/image-cache", 1),
                ("<data>/snapshots/s", 1),
                ("<scopes>", 1),
            ]),
            creates(&[
                ("<data>/vm-disks/<b256>/<vm>", 2),
                ("<data>/reclaim/1000/<b16>", 3),
                ("<data>/vm-disks/<b256>", 1),
                ("<scopes>", 1),
            ]),
            creates(&[
                ("<data>/vm-disks/<b256>/<vm>", 2),
                ("<data>/reclaim/1000/<b16>", 3),
                ("<data>/vm-disks/<b256>", 1),
                ("<data>/snapshots/s", 2),
                ("<scopes>", 1),
            ]),
        ];
        assert_eq!(
            judge_creates(&starts, &[]),
            [
                CreateFinding::Shared(SharedCreate {
                    dir: "<data>/reclaim/1000/<b16>".to_string(),
                    per_start: vec![3, 3, 3],
                    fan_out: 16,
                }),
                CreateFinding::Shared(SharedCreate {
                    dir: "<data>/snapshots/s".to_string(),
                    per_start: vec![1, 0, 2],
                    fan_out: 1,
                }),
            ]
        );
    }

    #[test]
    fn an_allowed_shared_directory_is_not_reported_until_a_start_stops_writing_there() {
        let allowance = CreateAllowance {
            dir: "<data>/snapshots/s",
            issue: 1,
        };
        let shared = vec![
            creates(&[("<data>/snapshots/s", 1)]),
            creates(&[("<data>/snapshots/s", 2)]),
        ];
        assert_eq!(judge_creates(&shared, &[allowance]), []);
        let not_shared = vec![creates(&[("<data>/snapshots/s", 1)]), creates(&[])];
        assert_eq!(
            judge_creates(&not_shared, &[allowance]),
            [CreateFinding::StaleAllowance {
                dir: "<data>/snapshots/s",
                issue: 1
            }]
        );
    }

    #[test]
    fn directories_are_named_alike_in_both_starts() {
        let roots = roots();
        for (dir, key) in [
            ("/mnt/store/dl-1", "<data>"),
            ("/mnt/store/dl-1/state", "<data>/state"),
            ("/mnt/store/dl-12/state", "/mnt/store/dl-12/state"),
            ("/sys/fs/cgroup/fcvm-dl-1.slice", "<scopes>"),
            (
                "/sys/fs/cgroup/fcvm-dl-1.slice/fcvm-7-9.scope",
                "<scopes>/fcvm-7-9.scope",
            ),
            ("/sys/fs/cgroup", "/sys/fs/cgroup"),
            ("/proc", "/proc"),
            ("/proc/4181/fd", "/proc/<pid>/fd"),
            ("/proc/4181/task/4182/fd", "/proc/<pid>/task/<pid>/fd"),
            ("/proc/sys/net", "/proc/sys/net"),
            ("/dev/shm/fcvm-1000", "/dev/shm/fcvm-1000"),
        ] {
            assert_eq!(dir_key(dir, &roots), key, "{dir}");
        }
    }

    #[test]
    fn the_reads_of_one_name_are_summed() {
        let mut trace = Trace::default();
        for (dir, entries, reader) in [
            ("/proc/4181/fd", 6, "fcvm"),
            ("/proc/4190/fd", 9, "pasta"),
            ("/mnt/store/dl-1/state", 4, "fcvm"),
        ] {
            trace.dirs.insert(
                dir.to_string(),
                DirRead {
                    entries,
                    readers: BTreeSet::from([reader.to_string()]),
                },
            );
        }
        let reads = reads_of(&trace, &roots());
        assert_eq!(reads["/proc/<pid>/fd"].entries, 15);
        assert_eq!(
            reads["/proc/<pid>/fd"].readers,
            BTreeSet::from(["fcvm".to_string(), "pasta".to_string()])
        );
        assert_eq!(reads["<data>/state"].entries, 4);
    }

    #[test]
    fn a_directory_with_an_entry_per_vm_is_found_on_any_filesystem() {
        for dir in [
            "<data>/state",
            "<scopes>",
            "/dev/shm/fcvm-1000/run",
            "/proc",
        ] {
            let few = reads(&[(dir, 3), ("/proc/<pid>/fd", 12)]);
            let many = reads(&[(dir, 8), ("/proc/<pid>/fd", 12)]);
            assert_eq!(
                judge(&few, &many, 5, &[]),
                [Finding::Grows {
                    dir: dir.to_string(),
                    beside_few: 3,
                    beside_many: 8
                }],
                "{dir}"
            );
        }
    }

    #[test]
    fn fewer_new_entries_than_vms_added_is_not_a_listing_per_vm() {
        // Four processes nobody here started came up between the two starts.
        let few = reads(&[("/proc", 400), ("<data>/snapshots", 3)]);
        let many = reads(&[("/proc", 404), ("<data>/snapshots", 3)]);
        assert_eq!(judge(&few, &many, 5, &[]), []);
        // The same directory read from a host that got quieter.
        assert_eq!(judge(&many, &few, 5, &[]), []);
    }

    #[test]
    fn a_visit_to_a_directory_of_every_sibling_is_found() {
        // Each start lists its own VM directory. The second also lists the five added.
        let few = reads(&[("<data>/vm-disks/vm-a/disks", 3)]);
        let many = reads(&[
            ("<data>/vm-disks/vm-b/disks", 3),
            ("<data>/vm-disks/vm-c/disks", 3),
            ("<data>/vm-disks/vm-d/disks", 3),
            ("<data>/vm-disks/vm-e/disks", 3),
            ("<data>/vm-disks/vm-f/disks", 3),
            ("<data>/vm-disks/vm-g/disks", 3),
        ]);
        assert_eq!(
            judge(&few, &many, 5, &[]),
            [Finding::Grows {
                dir: LISTED_BY_ONE_START.to_string(),
                beside_few: 3,
                beside_many: 18
            }]
        );
        // Its own directory alone, under another id each time, is no growth.
        let own = reads(&[("<data>/vm-disks/vm-b/disks", 3)]);
        assert_eq!(judge(&few, &own, 5, &[]), []);
    }

    #[test]
    fn a_directory_only_the_second_start_listed_is_found() {
        let few = reads(&[("/proc/<pid>/fd", 12)]);
        let many = reads(&[("/proc/<pid>/fd", 12), ("<data>/state", 9)]);
        assert_eq!(
            judge(&few, &many, 5, &[]),
            [Finding::Grows {
                dir: LISTED_BY_ONE_START.to_string(),
                beside_few: 0,
                beside_many: 9
            }]
        );
    }

    #[test]
    fn an_allowance_covers_its_directory_and_no_other() {
        let allowance = Allowance {
            dir: "<scopes>",
            issue: 2178,
        };
        let few = reads(&[("<scopes>", 70), ("<data>/state", 3)]);
        let many = reads(&[("<scopes>", 75), ("<data>/state", 8)]);
        assert_eq!(
            judge(&few, &many, 5, &[allowance]),
            [Finding::Grows {
                dir: "<data>/state".to_string(),
                beside_few: 3,
                beside_many: 8
            }]
        );
    }

    #[test]
    fn an_allowance_whose_directory_no_longer_grows_fails() {
        let allowance = Allowance {
            dir: "<scopes>",
            issue: 2178,
        };
        let stale = |beside_few, beside_many| {
            [Finding::StaleAllowance {
                dir: "<scopes>",
                issue: 2178,
                beside_few,
                beside_many,
            }]
        };
        // Not listed at all.
        let quiet = reads(&[("/proc/<pid>/fd", 12)]);
        assert_eq!(judge(&quiet, &quiet, 5, &[allowance]), stale(0, 0));
        // Listed, with the same entries beside six as beside one.
        let flat = reads(&[("<scopes>", 70), ("/proc/<pid>/fd", 12)]);
        assert_eq!(judge(&flat, &flat, 5, &[allowance]), stale(70, 70));
        // Listed by the first start only: a sweep that was due, not a listing per start.
        assert_eq!(judge(&flat, &quiet, 5, &[allowance]), stale(70, 0));
    }

    #[test]
    fn an_allowance_for_a_directory_the_test_does_not_own_fails_and_covers_nothing() {
        for dir in ["/run/netns", "/proc", "<data>-other/state", "<scopesx>"] {
            let allowance = Allowance { dir, issue: 2194 };
            let few = reads(&[(dir, 4)]);
            let many = reads(&[(dir, 9)]);
            assert_eq!(
                judge(&few, &many, 5, &[allowance]),
                [
                    Finding::ForeignAllowance { dir, issue: 2194 },
                    Finding::Grows {
                        dir: dir.to_string(),
                        beside_few: 4,
                        beside_many: 9
                    }
                ],
                "{dir}"
            );
        }
        for dir in ["<data>", "<data>/reclaim/0", "<scopes>", "<scopes>/a.scope"] {
            assert!(owned_by_the_test(dir), "{dir}");
        }
    }

    #[test]
    fn the_table_shows_every_directory_of_both_starts() {
        let few = reads(&[("<data>/state", 3)]);
        let many = reads(&[("<data>/state", 8), ("/proc", 400)]);
        assert_eq!(
            table(&few, &many),
            concat!(
                "  beside few  beside many  directory (read by)\n",
                "           -          400  /proc (fcvm)\n",
                "           3            8  <data>/state (fcvm)\n",
            )
        );
    }
}

/// The detector against real processes: no VM, only strace.
mod detector {
    use super::*;

    /// A directory with `files` files in it, under `root`.
    fn dir_with(root: &Path, name: &str, files: usize) -> Result<PathBuf> {
        let dir = root.join(name);
        std::fs::create_dir(&dir)?;
        for index in 0..files {
            std::fs::write(dir.join(format!("vm-{index}.json")), b"{}")?;
        }
        Ok(dir)
    }

    /// The tracer reports a listing with its directory and its entry count, from a process
    /// two execs below the one it started, and reports nothing for a directory that was
    /// only written and read in. A tracer that saw nothing would pass every test below it.
    #[test]
    fn the_tracer_reports_a_listing_with_its_path_and_entry_count() -> Result<()> {
        require_tracer()?;
        let root = tempfile::tempdir()?;
        let listed = dir_with(root.path(), "listed", 3)?;
        let quiet = dir_with(root.path(), "quiet", 4)?;
        let script = format!(
            "ls {listed} >/dev/null; cat {quiet}/vm-0.json >/dev/null; : > {quiet}/made",
            listed = listed.display(),
            quiet = quiet.display()
        );
        let trace = trace_of(&["sh", "-c", &script])?;
        let under_root: Vec<(&String, &DirRead)> = trace
            .dirs
            .iter()
            .filter(|(dir, _)| Path::new(dir).starts_with(root.path()))
            .collect();
        // Three files, "." and "..".
        let expected = DirRead {
            entries: 5,
            readers: BTreeSet::from(["ls".to_string()]),
        };
        ensure!(
            under_root == [(&listed.display().to_string(), &expected)],
            "expected one listing of {} with 5 entries read by ls, got {under_root:?}",
            listed.display()
        );
        ensure!(trace.unnamed_entries == 0, "{trace:?}");
        let programs: Vec<&str> = trace
            .execs
            .iter()
            .filter_map(|(_, program)| Path::new(program).file_name()?.to_str())
            .collect();
        ensure!(
            programs == ["sh", "ls", "cat"],
            "expected the programs sh, ls and cat, got {:?}",
            trace.execs
        );
        let shell = trace.execs[0].0;
        ensure!(
            trace.execs[1..].iter().all(|(pid, _)| *pid != shell),
            "ls and cat ran in the shell's own process, so nothing shows that children are \
             followed: {:?}",
            trace.execs
        );
        Ok(())
    }

    /// The tracer reports the creates of real programs (a directory made, a file opened with
    /// O_CREAT, a rename) in the directories they land in, and nothing for an open of an
    /// existing file for reading. A tracer that saw no create would pass the start's rule.
    #[test]
    fn the_tracer_reports_each_create_in_its_directory() -> Result<()> {
        require_tracer()?;
        let root = tempfile::tempdir()?;
        let quiet = dir_with(root.path(), "quiet", 1)?;
        let script = format!(
            "mkdir {r}/made; : > {r}/made/file; mv {r}/made/file {r}/made/moved; cat {quiet}/vm-0.json >/dev/null",
            r = root.path().display(),
            quiet = quiet.display()
        );
        let trace = trace_of(&["sh", "-c", &script])?;
        let under_root: BTreeMap<&String, &u64> = trace
            .creates
            .iter()
            .filter(|(dir, _)| Path::new(dir).starts_with(root.path()))
            .collect();
        let (made, top) = (
            root.path().join("made").display().to_string(),
            root.path().display().to_string(),
        );
        ensure!(
            under_root == BTreeMap::from([(&top, &1), (&made, &2)]),
            "expected one create in {top} (the mkdir) and two in {made} (the file and the \
             rename), got {under_root:?}"
        );
        ensure!(trace.unplaced_creates == 0, "{trace:?}");
        // Named under the test's root as a start's creates are.
        let roots = [Root {
            name: "<data>",
            path: root.path().to_path_buf(),
        }];
        let named = creates_of(&trace, &roots);
        ensure!(
            named.get("<data>") == Some(&1) && named.get("<data>/made") == Some(&2),
            "expected <data> 1 and <data>/made 2, got:\n{}",
            create_table(std::slice::from_ref(&named))
        );
        Ok(())
    }

    /// The whole chain on a known defect: a helper that lists a directory holding an entry
    /// per "VM" is run beside one and beside six, and the comparison finds that directory.
    /// A helper that only creates and reads entries of a directory like it is run the same
    /// way and nothing is found. The comparison can fail, and fails for the right directory.
    #[test]
    fn a_helper_that_lists_a_directory_of_vms_is_caught_and_one_that_does_not_is_not() -> Result<()>
    {
        require_tracer()?;
        let root = tempfile::tempdir()?;
        let roots = [Root {
            name: "<data>",
            path: root.path().to_path_buf(),
        }];
        let state = dir_with(root.path(), "state", 1)?;
        let owners = dir_with(root.path(), "owners", 1)?;
        let script = format!(
            "ls {state} >/dev/null; : > {owners}/made; cat {owners}/vm-0.json >/dev/null",
            state = state.display(),
            owners = owners.display()
        );
        let run = || -> Result<Reads> { Ok(reads_of(&trace_of(&["sh", "-c", &script])?, &roots)) };
        let few = run()?;
        for dir in [&state, &owners] {
            for index in 1..6 {
                std::fs::write(dir.join(format!("vm-{index}.json")), b"{}")?;
            }
        }
        let many = run()?;
        let found = judge(&few, &many, 5, &[]);
        ensure!(
            found
                == [Finding::Grows {
                    dir: "<data>/state".to_string(),
                    beside_few: 3,
                    beside_many: 8,
                }],
            "expected the listing of <data>/state, and nothing else, to be found growing from \
             3 entries to 8; got {found:?}\n{}",
            table(&few, &many)
        );
        Ok(())
    }
}

/// The invariant on real starts. Root only: an unprivileged strace takes the setuid bit from
/// `newuidmap` and `newgidmap` (the kernel does not honour it in a traced process), and a
/// rootless start would then write the single-id map, which is not the path it takes
/// untraced. Root's strace leaves the bit alone, so it traces root's own starts and, with
/// `-u`, an unprivileged user's.
#[cfg(all(feature = "integration-slow", feature = "privileged-tests"))]
mod start {
    use super::*;
    use std::time::{Duration, Instant};

    /// The clones alive when the first measured start begins, and the clones added before
    /// the second: the first measured clone itself and four more.
    const ALIVE_AT_FIRST: usize = 1;
    const ADDED: usize = 5;

    const HEALTHY: &str = "VM healthy, switching to";

    type Running = Vec<(tokio::process::Child, u32)>;

    /// The unprivileged user a run's fcvm processes belong to, while the test and strace
    /// are root.
    #[derive(Debug, Clone)]
    struct User {
        uid: u32,
        gid: u32,
        name: String,
        home: String,
    }

    impl User {
        /// The user who ran the root tests through sudo.
        fn invoker() -> Result<Self> {
            let uid: u32 = std::env::var("SUDO_UID")
                .context(
                    "SUDO_UID is not set: this test starts fcvm as the user who ran the root \
                     tests through sudo (make test-root)",
                )?
                .parse()
                .context("SUDO_UID")?;
            ensure!(
                uid != 0,
                "SUDO_UID is 0: the root tests were not run by a user"
            );
            let output = std::process::Command::new("getent")
                .args(["passwd", &uid.to_string()])
                .output()
                .context("running getent")?;
            let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
            // name:password:uid:gid:gecos:home:shell
            let fields: Vec<&str> = line.split(':').collect();
            ensure!(
                output.status.success() && fields.len() == 7,
                "getent passwd {uid} answered {line:?}"
            );
            Ok(Self {
                uid,
                gid: fields[3].parse().context("the user's gid")?,
                name: fields[0].to_string(),
                home: fields[5].to_string(),
            })
        }
    }

    /// Whose the fcvm processes of one run are: the test's own user, or `user`.
    struct Runner {
        user: Option<User>,
    }

    impl Runner {
        /// The command an fcvm that is not measured starts under. An fcvm of `user` gets its
        /// parent-death signal back from setpriv: a change of user id clears the one the
        /// test helper armed.
        fn untraced(&self) -> Vec<String> {
            let Some(user) = &self.user else {
                return Vec::new();
            };
            [
                "setpriv",
                "--reuid",
                &user.uid.to_string(),
                "--regid",
                &user.gid.to_string(),
                "--init-groups",
                "--pdeathsig",
                "KILL",
            ]
            .iter()
            .map(|arg| arg.to_string())
            .collect()
        }

        /// The command a measured fcvm starts under: strace, logging to `log`.
        fn traced(&self, log: &Path) -> Vec<String> {
            let mut command = vec![String::from("strace")];
            command.extend(tracer_args(log));
            if let Some(user) = &self.user {
                // strace stays root and runs the command as the user, so the user's setuid
                // helpers keep their bit. setpriv arms the parent-death signal again.
                command.extend(
                    ["-u", &user.name, "setpriv", "--pdeathsig", "KILL"]
                        .iter()
                        .map(|arg| arg.to_string()),
                );
            }
            command
        }

        /// The environment of the user's own login, where the test's is root's.
        fn env(&self) -> Vec<(&str, &str)> {
            match &self.user {
                Some(user) => vec![
                    ("HOME", user.home.as_str()),
                    ("USER", user.name.as_str()),
                    ("LOGNAME", user.name.as_str()),
                ],
                None => Vec::new(),
            }
        }
    }

    /// Which UID and GID maps a start logged for its namespace holder, if it made one.
    fn id_map_of(log: &Path) -> Result<Option<&'static str>> {
        let text =
            std::fs::read_to_string(log).with_context(|| format!("reading {}", log.display()))?;
        Ok([
            "extended UID/GID mappings (0-65535)",
            "single UID/GID mapping (fallback)",
            "UID/GID mappings written by root with the sudo invoker at 65536",
        ]
        .into_iter()
        .find(|line| text.contains(line)))
    }

    /// The directories a start is still known to list once per VM alive. Each entry names
    /// its issue and comes out with the fix: [`judge`] fails an entry whose directory no
    /// longer grows, and one for a directory this test does not own. There is none.
    fn allowances() -> Vec<Allowance> {
        Vec::new()
    }

    /// The sweeps a start runs once an interval (`fcvm::sweep`), each held off for longer
    /// than this test runs, so that no measured start is the one that sweeps: the base
    /// VM's launch finds no stamp, sweeps and stamps. `*=0` makes any sweep that is not
    /// named here run in every start, where it lists its table and fails this test until
    /// it is named.
    const SWEEPS_HELD_OFF: &str =
        "*=0,state=3600000,reclaim=3600000,cgroup-scopes=3600000,routed-leases=3600000";

    async fn wait_for_line(
        log: &Path,
        needle: &str,
        child: &mut tokio::process::Child,
    ) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            if std::fs::read_to_string(log).is_ok_and(|text| text.contains(needle)) {
                return Ok(());
            }
            if let Some(status) = child.try_wait()? {
                bail!(
                    "fcvm exited ({status}) before logging {needle:?}; log {}",
                    log.display()
                );
            }
            ensure!(
                Instant::now() < deadline,
                "no {needle:?} in {} after 300 s",
                log.display()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Start one clone of `snapshot`, under `wrapper` if any, and wait until it is healthy.
    /// The clone is in `running` from its spawn, so a start that fails is still stopped.
    async fn clone_until_healthy(
        wrapper: &[&str],
        snapshot: &str,
        name: &str,
        env: &[(&str, &str)],
        running: &mut Running,
    ) -> Result<(u32, PathBuf)> {
        let (child, pid, log) = common::spawn_fcvm_under_with_env_and_log_path(
            wrapper,
            &["snapshot", "run", "--snapshot", snapshot, "--name", name],
            env,
        )
        .await?;
        running.push((child, pid));
        let (child, _) = running.last_mut().expect("just pushed");
        wait_for_line(&log, HEALTHY, child).await?;
        Ok((pid, log))
    }

    /// The process tracing `pid`, which must be strace: a tracer that ended before the
    /// start was healthy has not logged the whole start.
    fn tracer_of(pid: u32) -> Result<u32> {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status"))
            .with_context(|| format!("reading /proc/{pid}/status"))?;
        let tracer: u32 = status
            .lines()
            .find_map(|line| line.strip_prefix("TracerPid:"))
            .context("no TracerPid in the process status")?
            .trim()
            .parse()
            .context("the TracerPid")?;
        ensure!(
            tracer != 0,
            "fcvm (pid {pid}) is not being traced: strace never attached or has ended, so \
             its log does not hold the whole start"
        );
        let comm = std::fs::read_to_string(format!("/proc/{tracer}/comm"))
            .with_context(|| format!("reading the tracer's (pid {tracer}) command name"))?;
        ensure!(
            comm.trim() == "strace",
            "fcvm (pid {pid}) is traced by {:?}, not by strace",
            comm.trim()
        );
        Ok(tracer)
    }

    /// Start one clone under strace and return the directories it listed from its exec
    /// until it was healthy, with the entries read from each, and the directories it created
    /// entries in. The clones already running stay up, and so does this one: strace stays
    /// attached to it until it exits. `untraced_map` is the id map an untraced start of the
    /// same user logged.
    async fn listings_of_one_start(
        runner: &Runner,
        snapshot: &str,
        name: &str,
        env: &[(&str, &str)],
        roots: &[Root],
        untraced_map: Option<&'static str>,
        running: &mut Running,
    ) -> Result<(Reads, Creates)> {
        std::fs::create_dir_all(common::TEST_LOG_DIR)?;
        let log = Path::new(common::TEST_LOG_DIR).join(format!("{name}.strace"));
        let wrapper = runner.traced(&log);
        let wrapper: Vec<&str> = wrapper.iter().map(String::as_str).collect();
        let (pid, fcvm_log) = clone_until_healthy(&wrapper, snapshot, name, env, running).await?;
        tracer_of(pid)?;
        let text = std::fs::read(&log).with_context(|| format!("reading {}", log.display()))?;
        let trace = parse_trace(&String::from_utf8_lossy(&text))
            .with_context(|| format!("reading {}", log.display()))?;

        // The log must be of this start and of the processes it created.
        let fcvm = common::find_fcvm_binary()?;
        let became_fcvm = trace
            .execs
            .iter()
            .find(|(_, program)| Path::new(program).file_name() == fcvm.file_name());
        ensure!(
            became_fcvm.is_some_and(|(first, _)| *first == pid),
            "{} does not show pid {pid} becoming {}: {:?}",
            log.display(),
            fcvm.display(),
            trace.execs
        );
        ensure!(
            trace.execs.iter().any(|(other, _)| *other != pid),
            "{} shows no program started by a process fcvm created (a holder, pasta, the \
             VMM): strace did not follow the start's children",
            log.display()
        );
        ensure!(
            trace.unnamed_entries == 0,
            "{} holds {} directory entries read through descriptors strace printed no path \
             for, so the directories listed are not all known",
            log.display(),
            trace.unnamed_entries
        );
        ensure!(
            trace.unplaced_creates == 0,
            "{} holds {} entry-creating calls whose directory the log does not name, so the \
             directories a start writes into are not all known",
            log.display(),
            trace.unplaced_creates
        );
        // Being traced must not change how the start sets its namespace up.
        let traced_map = id_map_of(&fcvm_log)?;
        ensure!(
            traced_map == untraced_map,
            "the traced start {name} logged the id map {traced_map:?} and an untraced start \
             beside it {untraced_map:?}: strace changed the path the start takes (log {})",
            fcvm_log.display()
        );
        println!(
            "start {name} (pid {pid}): id map {traced_map:?}, strace log {}",
            log.display()
        );
        Ok((reads_of(&trace, roots), creates_of(&trace, roots)))
    }

    /// The store directories every start is still known to create an entry in. Each entry
    /// names its issue and comes out with the fix ([`judge_creates`]).
    ///
    /// `fallback`: the starts keep their VM states in the runtime directory's fallback place,
    /// `<data>/runtime` (a user with no base).
    fn create_allowances(fallback: bool) -> Vec<CreateAllowance> {
        if !fallback {
            return Vec::new();
        }
        // The 16 state buckets, on the store only in the fallback place: production and the
        // rate measurements keep states under /run/fcvm-<uid> or a named base, on tmpfs, and
        // changing their count is a layout change the runtime directory's upgrade refusal
        // would have to cover for running VMs, which is not worth it for a fallback the
        // binding already tells the operator to replace (`sudo fcvm setup --runtime-dir`).
        [
            "<data>/runtime/boot-<id>/state/vms/<b16>",
            "<data>/runtime/boot-<id>/state/names/<b16>",
            "<data>/runtime/boot-<id>/state/pids/<b16>",
        ]
        .map(|dir| CreateAllowance { dir, issue: 2240 })
        .into()
    }

    /// Remove the scopes' parent the starts made, with whatever empty scopes are left in it.
    async fn remove_scopes_parent(parent: &Path) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let Ok(entries) = std::fs::read_dir(parent) else {
                return Ok(());
            };
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    let _ = std::fs::remove_dir(entry.path());
                }
            }
            match std::fs::remove_dir(parent) {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => ensure!(
                    Instant::now() < deadline,
                    "the test's cgroup {} is still there 10 s after its VMs were stopped: \
                     {error}",
                    parent.display()
                ),
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn the_entries_a_start_lists_do_not_grow(network: &str, runner: Runner) -> Result<()> {
        require_tracer()?;
        // SAFETY: geteuid has no preconditions.
        ensure!(
            unsafe { libc::geteuid() } == 0,
            "this test traces its starts as root (see the module doc); run it with make \
             test-root"
        );
        // Short (`common::short_data_dir`): a VM's sockets sit under the data directory.
        // strace prints the kernel's path of a descriptor, so the data directory is named
        // without symbolic links.
        let id = &uuid::Uuid::new_v4().simple().to_string()[..8];
        // A user's data directory is beside the store's own, where the user can reach it:
        // the root tests' data directory is root's.
        let store = match &runner.user {
            Some(_) => fcvm::paths::assets_dir(),
            None => fcvm::paths::data_dir(),
        };
        std::fs::create_dir_all(&store).with_context(|| format!("creating {}", store.display()))?;
        let data_dir = common::short_data_dir(
            &store
                .canonicalize()
                .context("resolving the data directory")?,
            "dl",
        );
        let data_dir_str = data_dir.to_str().context("data dir path")?.to_string();
        if let Some(user) = &runner.user {
            std::fs::create_dir(&data_dir)
                .with_context(|| format!("creating {}", data_dir.display()))?;
            std::os::unix::fs::chown(&data_dir, Some(user.uid), Some(user.gid))
                .with_context(|| format!("giving {} to {}", data_dir.display(), user.name))?;
        }
        // The starts' cgroup scopes go under a parent of the test's own, so the entries of
        // that directory follow this test's VMs and no other test's.
        let scopes = format!("fcvm-dl-{id}.slice");
        let scopes_dir = Path::new(fcvm::cgroup::CGROUP2_ROOT).join(&scopes);
        let roots = [
            Root {
                name: "<data>",
                path: data_dir.clone(),
            },
            Root {
                name: "<scopes>",
                path: scopes_dir.clone(),
            },
        ];
        let mut env: Vec<(&str, &str)> = vec![
            ("FCVM_DATA_DIR", data_dir_str.as_str()),
            (fcvm::cgroup::PARENT_ENV, scopes.as_str()),
            (fcvm::sweep::EVERY_ENV, SWEEPS_HELD_OFF),
        ];
        env.extend(runner.env());
        let untraced = runner.untraced();
        let untraced: Vec<&str> = untraced.iter().map(String::as_str).collect();
        let who = runner
            .user
            .as_ref()
            .map_or("root", |user| user.name.as_str());
        let mut running = Running::new();
        let result = async {
            let (base, clone, snapshot, _) =
                common::unique_names(&format!("dirs-{network}-{}", runner.user.is_some()));
            let (child, pid, log) = common::spawn_fcvm_under_with_env_and_log_path(
                &untraced,
                &[
                    "podman",
                    "run",
                    "--name",
                    &base,
                    "--network",
                    network,
                    "--no-snapshot",
                    common::TEST_IMAGE,
                ],
                &env,
            )
            .await?;
            running.push((child, pid));
            wait_for_line(&log, HEALTHY, &mut running[0].0).await?;
            let fcvm = common::find_fcvm_binary()?;
            let mut create = match untraced.split_first() {
                Some((program, args)) => {
                    let mut create = tokio::process::Command::new(program);
                    create.args(args).arg(&fcvm);
                    create
                }
                None => tokio::process::Command::new(&fcvm),
            };
            let created = create
                .args([
                    "snapshot",
                    "create",
                    "--pid",
                    &pid.to_string(),
                    "--tag",
                    &snapshot,
                ])
                .envs(env.iter().copied())
                .kill_on_drop(true)
                .output()
                .await?;
            ensure!(
                created.status.success(),
                "snapshot create failed: {}",
                String::from_utf8_lossy(&created.stderr)
            );
            let (mut child, pid) = running.pop().expect("the base VM");
            common::kill_process(pid).await;
            let _ = child.wait().await;

            // The clones alive at the first measured start, then that start.
            let mut untraced_map = None;
            for index in 0..ALIVE_AT_FIRST {
                let (_, log) = clone_until_healthy(
                    &untraced,
                    &snapshot,
                    &format!("{clone}-a{index}"),
                    &env,
                    &mut running,
                )
                .await?;
                untraced_map = id_map_of(&log)?;
            }
            let scopes_entered = scopes_dir.is_dir();
            let (beside_few, first_creates) = listings_of_one_start(
                &runner,
                &snapshot,
                &format!("{clone}-m1"),
                &env,
                &roots,
                untraced_map,
                &mut running,
            )
            .await?;
            // The first measured clone is one of the clones added; these are the others. They
            // are traced too: every traced start's creates are compared ([`judge_creates`]).
            let mut creates = vec![first_creates];
            for index in 1..ADDED {
                let (_, made) = listings_of_one_start(
                    &runner,
                    &snapshot,
                    &format!("{clone}-b{index}"),
                    &env,
                    &roots,
                    untraced_map,
                    &mut running,
                )
                .await?;
                creates.push(made);
            }
            let (beside_many, last_creates) = listings_of_one_start(
                &runner,
                &snapshot,
                &format!("{clone}-m2"),
                &env,
                &roots,
                untraced_map,
                &mut running,
            )
            .await?;

            creates.push(last_creates);
            let listed = table(&beside_few, &beside_many);
            println!(
                "entries a {network} start by {who} read from each directory, beside \
                 {ALIVE_AT_FIRST} clone and beside {} (scopes entered: {scopes_entered}, id \
                 map: {untraced_map:?}):\n{listed}",
                ALIVE_AT_FIRST + ADDED
            );
            let found = judge(&beside_few, &beside_many, ADDED as u64, &allowances());
            let mut report = String::new();
            for finding in &found {
                report.push_str(&match finding {
                    Finding::Grows {
                        dir,
                        beside_few,
                        beside_many,
                    } => format!(
                        "a start read {beside_few} entries from {dir} beside {ALIVE_AT_FIRST} \
                         clone and {beside_many} beside {}: what a start lists there grows \
                         with the VMs alive\n",
                        ALIVE_AT_FIRST + ADDED
                    ),
                    Finding::ForeignAllowance { dir, issue } => format!(
                        "{dir} (#{issue}) is in `allowances` and is not a directory this \
                         test owns: only one under its data directory or its scopes' parent \
                         can be allowed\n"
                    ),
                    Finding::StaleAllowance {
                        dir,
                        issue,
                        beside_few,
                        beside_many,
                    } => format!(
                        "{dir} is allowed to grow with the VMs alive (#{issue}) and did not \
                         ({beside_few} entries, then {beside_many}): remove its entry from \
                         `allowances`\n"
                    ),
                });
            }
            let made = create_table(&creates);
            println!(
                "entry-creating calls of each of the {} traced {network} starts by {who}, by \
                 directory:\n{made}",
                creates.len()
            );
            let fallback = creates
                .iter()
                .any(|start| start.keys().any(|dir| dir.starts_with("<data>/runtime/")));
            let allowances = create_allowances(fallback);
            for allowance in &allowances {
                println!(
                    "allowed as a shared directory while states are in the fallback place \
                     <data>/runtime (#{}): {}; production keeps states on tmpfs under \
                     /run/fcvm-<uid> or a named base",
                    allowance.issue, allowance.dir
                );
            }
            for finding in judge_creates(&creates, &allowances) {
                report.push_str(&match finding {
                    CreateFinding::Shared(SharedCreate {
                        dir,
                        per_start,
                        fan_out: 1,
                    }) => format!(
                        "more than one start created an entry in {dir} ({per_start:?} calls): \
                         the starts that write into one directory take turns on its inode lock\n"
                    ),
                    CreateFinding::Shared(SharedCreate {
                        dir,
                        per_start,
                        fan_out,
                    }) => format!(
                        "every start created an entry in a bucket of {dir} ({per_start:?} calls), \
                         a family of {fan_out} buckets: each bucket's lock is shared by one start \
                         in {fan_out}, fewer than one in {MIN_FAN_OUT}\n"
                    ),
                    CreateFinding::StaleAllowance { dir, issue } => format!(
                        "{dir} is allowed as a directory the starts share (#{issue}) and the \
                         starts no longer share it: remove its entry from `create_allowances`\n"
                    ),
                });
            }
            ensure!(report.is_empty(), "{report}{listed}\n{made}");
            Ok(())
        }
        .await;
        for (mut child, pid) in running {
            common::kill_process(pid).await;
            let _ = child.wait().await;
        }
        let _ = tokio::fs::remove_dir_all(&data_dir).await;
        let removed = remove_scopes_parent(&scopes_dir).await;
        if let (Err(_), Err(left)) = (&result, &removed) {
            eprintln!("{left:#}");
        }
        result.and(removed)
    }

    /// Root's own start with rootless networking: the path that enters a cgroup scope.
    #[tokio::test]
    async fn a_start_lists_no_directory_that_grows_with_the_vms_alive_rootless() -> Result<()> {
        the_entries_a_start_lists_do_not_grow("rootless", Runner { user: None }).await
    }

    /// Root's own start with bridged networking.
    #[tokio::test]
    async fn a_start_lists_no_directory_that_grows_with_the_vms_alive_bridged() -> Result<()> {
        the_entries_a_start_lists_do_not_grow("bridged", Runner { user: None }).await
    }

    /// Root's own start with routed networking.
    #[tokio::test]
    async fn a_start_lists_no_directory_that_grows_with_the_vms_alive_routed() -> Result<()> {
        the_entries_a_start_lists_do_not_grow("routed", Runner { user: None }).await
    }

    /// An unprivileged user's start, traced by root's strace: the user's `newuidmap` and
    /// `newgidmap` keep their setuid bit, as in an untraced start.
    #[tokio::test]
    async fn a_start_lists_no_directory_that_grows_with_the_vms_alive_unprivileged() -> Result<()> {
        let user = User::invoker()?;
        the_entries_a_start_lists_do_not_grow("rootless", Runner { user: Some(user) }).await
    }
}
