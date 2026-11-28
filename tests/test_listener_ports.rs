//! Tests do not use the port of a listener they have closed (#2259).
//!
//! A test that binds a listener to port 0, reads its address and drops it holds nothing:
//! the kernel can give that port to any other socket on the host the next moment. In the
//! unit lane those sockets are other tests' listeners, so an address a test expects to
//! refuse can answer. `src/test_ports.rs` has what such a test wants instead, and is the
//! one file allowed to close a listener and hand out its port (`released_port`, for a
//! program under test that binds the port itself).
//!
//! The pin below is a lint over the three ways this tree wrote that, found when the
//! sites were converted. It is not a Rust parser: a new way to write the same thing is
//! not caught, and gets its shape added here when one is found.

// The library's `cfg(test)` module, by path, as tests/common/mod.rs takes it.
#[path = "../src/test_ports.rs"]
mod test_ports;

use regex::Regex;
use std::path::{Path, PathBuf};

/// The one file that may close a listener and hand out its port, and this file, whose
/// fixtures spell the shapes out.
const EXEMPT: [&str; 2] = ["src/test_ports.rs", "tests/test_listener_ports.rs"];

#[test]
fn nothing_else_can_listen_on_a_refused_address() {
    let (_held, refused) = test_ports::refused_address();
    let connect = std::net::TcpStream::connect(refused);
    assert_eq!(
        connect.map(|_| ()).map_err(|err| err.kind()),
        Err(std::io::ErrorKind::ConnectionRefused),
    );
    // What a test in another process does when the kernel hands it this port.
    let taken = std::net::TcpListener::bind(refused);
    assert_eq!(
        taken.map(|_| ()).map_err(|err| err.kind()),
        Err(std::io::ErrorKind::AddrInUse),
        "another socket could listen on an address a test expects to be refused",
    );
}

/// Source with every run of whitespace reduced to one space and none before a `.`, so a
/// method chain reads the same however rustfmt broke it.
fn flattened(source: &str) -> String {
    let one_space = source.split_whitespace().collect::<Vec<_>>().join(" ");
    one_space.replace(" .", ".")
}

/// Where `source` binds a socket only to take its address, as (shape, excerpt).
fn listeners_bound_for_their_address(source: &str) -> Vec<(&'static str, String)> {
    let flat = flattened(source);
    let excerpt = |text: &str| text.chars().take(120).collect::<String>();
    let mut found = Vec::new();

    // One statement binds and takes the address: the listener is a temporary, closed at
    // the end of the statement.
    //     let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    for statement in flat.split([';', '{', '}']) {
        if let Some(bind) = statement.find("bind(") {
            if statement[bind..].contains("local_addr()") {
                found.push((
                    "bound and read in one statement",
                    excerpt(&statement[bind..]),
                ));
            }
        }
    }

    // A block binds, and its value is the address: the listener is closed at the brace.
    //     let port = { let l = TcpListener::bind("127.0.0.1:0").unwrap(); l.local_addr().unwrap().port() };
    let block =
        Regex::new(r"\{ let (\w+) = [^;{}]*\bbind\([^;{}]*; (\w+)\.local_addr\(\)[^;{}]*\}")
            .unwrap();
    for hit in block.captures_iter(&flat) {
        if hit[1] == hit[2] {
            found.push(("bound in a block that yields its address", excerpt(&hit[0])));
        }
    }

    // A listener is bound, asked for its address and dropped, with nothing in between:
    // whatever uses the address comes after the listener is closed.
    //     let l = TcpListener::bind("127.0.0.1:0")?; let addr = l.local_addr()?; drop(l);
    let binding = Regex::new(r"let (\w+) = [^;{}]*\bbind\([^;{}]*;").unwrap();
    for hit in binding.captures_iter(&flat) {
        let name = &hit[1];
        let after = &flat[hit.get(0).unwrap().end()..];
        let Some(dropped) = after.find(&format!("drop({name})")) else {
            continue;
        };
        let read = format!("{name}.local_addr()");
        let mut between = after[..dropped]
            .split(';')
            .map(str::trim)
            .filter(|statement| !statement.is_empty())
            .peekable();
        if between.peek().is_some() && between.all(|statement| statement.contains(&read)) {
            found.push(("bound, read and dropped", excerpt(&hit[0])));
        }
    }
    found
}

fn rust_sources(dir: &Path, into: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap_or_else(|err| panic!("{}: {err}", dir.display())) {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        // Not followed: `target` is a link out of the tree.
        let kind = std::fs::symlink_metadata(&path).unwrap().file_type();
        if kind.is_dir() {
            if !name.starts_with('.') && name != "target" && name != "node_modules" {
                rust_sources(&path, into);
            }
        } else if kind.is_file() && name.ends_with(".rs") {
            into.push(path);
        }
    }
}

#[test]
fn no_listener_is_bound_only_for_its_address() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_sources(&root, &mut files);
    // A walk that found next to nothing has checked nothing.
    assert!(
        files.len() > 300,
        "only {} Rust files under {}",
        files.len(),
        root.display()
    );
    let mut findings = Vec::new();
    let mut exempt_seen = 0;
    for path in &files {
        let relative = path
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .to_string();
        if EXEMPT.contains(&relative.as_str()) {
            exempt_seen += 1;
            continue;
        }
        let source = std::fs::read_to_string(path).unwrap();
        for (shape, text) in listeners_bound_for_their_address(&source) {
            findings.push(format!("{relative}: {shape}: {text}"));
        }
    }
    assert_eq!(exempt_seen, EXEMPT.len(), "an exempt file has moved");
    assert!(
        findings.is_empty(),
        "a listener bound only for its address is closed before the address is used, and \
         the kernel can give its port to another socket. Use test_ports::refused_address \
         for an address that must refuse, or test_ports::released_port for a program that \
         binds the port itself (src/test_ports.rs):\n{}",
        findings.join("\n")
    );
}

#[test]
fn the_pin_sees_each_shape_and_leaves_a_kept_listener_alone() {
    let shapes = |source: &str| -> Vec<&'static str> {
        listeners_bound_for_their_address(source)
            .into_iter()
            .map(|(shape, _)| shape)
            .collect()
    };
    // The three shapes, as the tree had them.
    assert_eq!(
        shapes(
            r#"let nobody = TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();"#
        ),
        ["bound and read in one statement"]
    );
    assert_eq!(
        shapes(
            r#"let ipv6_port = std::net::TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, 0))
                .and_then(|l| l.local_addr())
                .context("finding a free port on ::1")?
                .port();"#
        ),
        ["bound and read in one statement"]
    );
    assert_eq!(
        shapes(
            r#"let refused = {
                let gone = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                gone.local_addr().unwrap()
            };"#
        ),
        ["bound in a block that yields its address"]
    );
    assert_eq!(
        shapes(
            r#"let host_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let host_addr = host_listener.local_addr()?;
            drop(host_listener); // Release the port so bind_port_forwards can bind it"#
        ),
        ["bound, read and dropped"]
    );
    // A listener that goes on to accept is none of them, dropped later or not.
    assert_eq!(
        shapes(
            r#"let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = l.local_addr().unwrap().port();
            tokio::spawn(async move { axum::serve(l, app).await.unwrap() });"#
        ),
        [] as [&str; 0]
    );
    assert_eq!(
        shapes(
            r#"let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let (stream, _) = listener.accept().unwrap();
            drop(listener);"#
        ),
        [] as [&str; 0]
    );
    // Nor is one that is held, unanswering, while its address is probed.
    assert_eq!(
        shapes(
            r#"let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let error = probe("127.0.0.1", port).await.unwrap_err();
            drop(listener);"#
        ),
        [] as [&str; 0]
    );
    // A function that ends in a bind is not read by the function after it.
    assert_eq!(
        shapes(
            r#"fn server() -> Server {
                serve_from(TcpListener::bind("127.0.0.1:0").unwrap())
            }
            fn serve_from(l: TcpListener) -> Server {
                let port = l.local_addr().unwrap().port();
                Server { l, port }
            }"#
        ),
        [] as [&str; 0]
    );
    // A block that keeps its listener is not the second shape.
    assert_eq!(
        shapes(
            r#"let (l, port) = {
                let l = TcpListener::bind("127.0.0.1:0").unwrap();
                let port = l.local_addr().unwrap().port();
                (l, port)
            };"#
        ),
        [] as [&str; 0]
    );
}
