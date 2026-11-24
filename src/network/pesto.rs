//! pasta's configuration protocol ("pesto", passt's `pesto.h` and `conf.c`), as far as
//! fcvm speaks it: a reset of what pasta holds about one guest, over the control socket of
//! a pasta that stays with its namespace (`pasta -c`).
//!
//! On a connection pasta sends a hello and its current forwarding tables. The client sends
//! its own tables, each led by its interface's id and the list ended by a zero byte, and
//! pasta applies them and closes. The patches fcvm carries
//! (`scripts/passt-control-reset-status.patch`) add two things this module relies on:
//!
//! - a client may send [`RESET`] before its tables: pasta closes every flow it has (TCP,
//!   spliced TCP, UDP, ping), sending nothing to the guest's side, and takes back the guest
//!   MAC and addresses it learned from traffic;
//! - pasta answers with one status byte before it closes, for a reset once the flows are
//!   closed. Bit 0 says a forwarding rule could not be bound.
//!
//! The reset here sends no tables, which also ends every forward pasta had.

use anyhow::{bail, ensure, Context, Result};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// `PESTO_SERVER_MAGIC`.
const MAGIC: &[u8; 8] = b"basil:s\0";
/// `PESTO_PROTOCOL_VERSION` of the pinned passt. Another version has other messages.
const VERSION: u32 = 2;
/// `PIF_NAME_SIZE` and `IFNAMSIZ`, which size the messages below.
const PIF_NAME_SIZE: u32 = 128;
const IFNAMSIZ: u32 = 16;
/// `sizeof(struct fwd_rule)`: two 16-byte addresses, the interface name, three ports, the
/// protocol and the flags.
const RULE_SIZE: usize = 16 + 16 + IFNAMSIZ as usize + 2 + 2 + 2 + 1 + 1;
/// `PESTO_RESET`, sent where an interface id would be.
const RESET: u8 = 0xff;
/// The end of the client's tables, and of pasta's (`PIF_NONE`).
const END: u8 = 0;
/// `PESTO_STATUS_LISTEN_FAILED`.
const STATUS_LISTEN_FAILED: u8 = 0x01;
/// A reset is one loop of pasta's event handler; this is for a pasta that has stopped.
const BUDGET: Duration = Duration::from_secs(5);

/// Have the pasta listening on `control` forget its guest: every flow closed, the guest's
/// MAC and addresses as pasta started with them, no forwards. Returns once pasta says it
/// is done.
pub async fn reset(control: &Path) -> Result<()> {
    tokio::time::timeout(BUDGET, exchange(control, &[RESET, END]))
        .await
        .with_context(|| {
            format!(
                "pasta did not answer on {} in {BUDGET:?}",
                control.display()
            )
        })?
        .with_context(|| format!("resetting pasta over {}", control.display()))
}

/// One connection: read pasta's hello and tables, send `request`, read the status byte.
async fn exchange(control: &Path, request: &[u8]) -> Result<()> {
    let mut stream = UnixStream::connect(control)
        .await
        .context("connecting to the control socket")?;
    let mut hello = [0u8; 20];
    stream
        .read_exact(&mut hello)
        .await
        .context("reading pasta's hello")?;
    let word = |at: usize| u32::from_be_bytes(hello[at..at + 4].try_into().expect("four bytes"));
    ensure!(
        &hello[..8] == MAGIC,
        "not a pasta control socket: {:?}",
        &hello[..8]
    );
    ensure!(
        (word(8), word(12), word(16)) == (VERSION, PIF_NAME_SIZE, IFNAMSIZ),
        "pasta speaks configuration protocol {} (names of {} and {} bytes); this client \
         speaks {VERSION} ({PIF_NAME_SIZE} and {IFNAMSIZ})",
        word(8),
        word(12),
        word(16)
    );
    // pasta's own tables: an interface id, its name, capabilities and rule count, then the
    // rules; a zero id ends them. Nothing here uses them, and pasta reads no request
    // before it has written them.
    loop {
        let id = stream.read_u8().await.context("reading pasta's tables")?;
        if id == END {
            break;
        }
        let mut info = [0u8; PIF_NAME_SIZE as usize + 8];
        stream
            .read_exact(&mut info)
            .await
            .context("reading an interface's entry")?;
        let count = u32::from_be_bytes(info[info.len() - 4..].try_into().expect("four bytes"));
        ensure!(count <= 255, "pasta lists {count} rules for one interface");
        let mut rules = vec![0u8; count as usize * RULE_SIZE];
        stream
            .read_exact(&mut rules)
            .await
            .context("reading an interface's rules")?;
    }
    stream
        .write_all(request)
        .await
        .context("sending the request")?;
    // A pasta without the carried patch closes here with no status byte: an error, since
    // nothing then says the reset happened.
    let status = stream
        .read_u8()
        .await
        .context("reading pasta's status (a pasta without fcvm's patches sends none)")?;
    if status & STATUS_LISTEN_FAILED != 0 {
        bail!("pasta could not bind a forwarding rule");
    }
    ensure!(status == 0, "pasta answered with status {status:#x}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    /// A stand-in pasta: sends `greeting`, reads the request, answers `status`.
    async fn serve(listener: UnixListener, greeting: Vec<u8>, status: Option<u8>) -> Vec<u8> {
        let (mut stream, _) = listener.accept().await.unwrap();
        stream.write_all(&greeting).await.unwrap();
        let mut request = [0u8; 2];
        let read = stream
            .read_exact(&mut request)
            .await
            .map(|_| 2)
            .unwrap_or(0);
        if let Some(status) = status {
            let _ = stream.write_all(&[status]).await;
        }
        request[..read].to_vec()
    }

    fn hello(version: u32) -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        for word in [version, PIF_NAME_SIZE, IFNAMSIZ] {
            bytes.extend_from_slice(&word.to_be_bytes());
        }
        bytes
    }

    /// An interface with `rules` forwarding rules, as pasta lists it.
    fn table(id: u8, rules: u32) -> Vec<u8> {
        let mut bytes = vec![id];
        bytes.extend_from_slice(&[b'H'; PIF_NAME_SIZE as usize]);
        bytes.extend_from_slice(&0x3fu32.to_be_bytes());
        bytes.extend_from_slice(&rules.to_be_bytes());
        bytes.extend(std::iter::repeat_n(0xaa, rules as usize * RULE_SIZE));
        bytes
    }

    async fn run(greeting: Vec<u8>, status: Option<u8>) -> (Result<()>, Vec<u8>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let pasta = tokio::spawn(serve(listener, greeting, status));
        let outcome = reset(&path).await;
        (outcome, pasta.await.unwrap())
    }

    /// The reset reads past pasta's tables, whatever they hold, sends the reset marker and
    /// the end of an empty table list, and takes a zero status as done.
    #[tokio::test]
    async fn a_reset_is_the_marker_after_pastas_tables_and_a_zero_status() {
        let mut greeting = hello(VERSION);
        greeting.extend(table(1, 0));
        greeting.extend(table(3, 2));
        greeting.push(END);
        let (outcome, request) = run(greeting, Some(0)).await;
        outcome.unwrap();
        assert_eq!(request, [RESET, END]);
    }

    /// A pasta that closes without a status (one without the carried patch), reports a
    /// rule it could not bind, or speaks another protocol version is an error: nothing
    /// then says its flows are gone.
    #[tokio::test]
    async fn a_reset_pasta_did_not_confirm_is_an_error() {
        let mut plain = hello(VERSION);
        plain.push(END);
        let (outcome, _) = run(plain.clone(), None).await;
        assert!(format!("{:#}", outcome.unwrap_err()).contains("status"));
        let (outcome, _) = run(plain.clone(), Some(STATUS_LISTEN_FAILED)).await;
        assert!(format!("{:#}", outcome.unwrap_err()).contains("could not bind"));
        let (outcome, _) = run(plain, Some(0x80)).await;
        assert!(format!("{:#}", outcome.unwrap_err()).contains("0x80"));

        let mut other = hello(3);
        other.push(END);
        let (outcome, request) = run(other, Some(0)).await;
        assert!(format!("{:#}", outcome.unwrap_err()).contains("protocol 3"));
        assert_eq!(
            request, b"",
            "no request goes to a pasta of another version"
        );
    }
}
