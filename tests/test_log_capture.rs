//! The test log capture must be complete before a test reads it.
//!
//! `spawn_fcvm_with_env_and_log_path` copies a child's stdout and stderr into a
//! log file from two detached tasks. Tests assert on that file after the child
//! exits, and a capture that is silently incomplete turns an absence check into
//! a vacuous pass. Two ways it was incomplete, each pinned here by a test that
//! failed before its fix:
//!
//! - one unreadable line (invalid UTF-8 from a guest console) ended the copy
//!   loop, dropping everything after it, and the end-of-stream marker then
//!   certified the truncated file as complete;
//! - `wait_for_log_eof` matched the marker as a substring, so a child line that
//!   quoted it (or an argv that contained it) released the waiter while the
//!   stream was still draining.

mod common;

use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// Ends when the log holds an exact `marker` line, or panics after `timeout`.
async fn wait_for_line(path: &std::path::Path, marker: &str, timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if text.lines().any(|l| l == marker) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no line {marker:?} in {} after {timeout:?}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn log_capture_continues_past_an_unreadable_line() {
    let logger = common::TestLogger::new("log-capture-utf8");
    let path = logger.path().clone();
    // Line 2 is not UTF-8. Before the fix the copy loop treated the resulting
    // read error as end-of-stream and line 3 never reached the file.
    let stdout: &'static [u8] = b"before-the-bad-line\n\xff\xfe\n after-the-bad-line\n";
    let stderr: &'static [u8] = b"";
    common::spawn_log_consumer_to_file(Some(stdout), "utf8", Some(logger.clone()), false);
    common::spawn_log_consumer_to_file(Some(stderr), "utf8", Some(logger), true);

    common::wait_for_log_eof(&path, Duration::from_secs(5))
        .await
        .expect("both consumers reach end of stream");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("after-the-bad-line"),
        "the line after the unreadable one was dropped; the capture is truncated \
         and the end-of-stream marker certified it anyway:\n{text}"
    );
    assert!(
        text.contains("before-the-bad-line"),
        "the line before the unreadable one is missing:\n{text}"
    );
}

/// A reader that serves one line and then fails with a non-UTF-8 error.
struct TornPipe {
    served: bool,
}

impl tokio::io::AsyncRead for TornPipe {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let me = self.get_mut();
        if !me.served {
            me.served = true;
            buf.put_slice(b"only line before the tear\n");
            std::task::Poll::Ready(Ok(()))
        } else {
            std::task::Poll::Ready(Err(std::io::Error::other("pipe torn")))
        }
    }
}

#[tokio::test]
async fn wait_for_log_eof_reports_a_failed_capture_instead_of_certifying_it() {
    let logger = common::TestLogger::new("log-capture-torn");
    let path = logger.path().clone();
    let stderr: &'static [u8] = b"";
    common::spawn_log_consumer_to_file(
        Some(TornPipe { served: false }),
        "torn",
        Some(logger.clone()),
        false,
    );
    common::spawn_log_consumer_to_file(Some(stderr), "torn", Some(logger), true);

    // A read error is not end-of-stream: whatever the child wrote after it
    // was never captured. Certifying that file complete lets an absence
    // check pass against a truncated log, so the waiter must fail instead.
    let result = common::wait_for_log_eof(&path, Duration::from_secs(5)).await;
    let text = std::fs::read_to_string(&path).unwrap();
    let err = match result {
        Ok(()) => panic!(
            "wait_for_log_eof certified a capture whose stream failed mid-way as complete:\n{text}"
        ),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("pipe torn"),
        "the waiter's error must carry the read error, not a timeout:\n{err}\n{text}"
    );
    assert!(
        text.contains("only line before the tear"),
        "the lines before the failure must still be in the file:\n{text}"
    );
}

#[tokio::test]
async fn wait_for_log_eof_ignores_a_child_line_that_quotes_the_marker() {
    let logger = common::TestLogger::new("log-capture-quoted-marker");
    let path = logger.path().clone();
    let (reader, mut writer) = tokio::io::duplex(256);
    let stderr: &'static [u8] = b"";
    common::spawn_log_consumer_to_file(Some(reader), "quoter", Some(logger.clone()), false);
    common::spawn_log_consumer_to_file(Some(stderr), "quoter", Some(logger), true);

    // The child prints the marker's text as ordinary output, then keeps going.
    // A substring match releases the waiter here; the real marker is a line of
    // its own, written only when this stream closes.
    writer
        .write_all(b"[fcvm-test] child stdout reached end of stream\n")
        .await
        .unwrap();
    wait_for_line(
        &path,
        "[quoter] [fcvm-test] child stdout reached end of stream",
        Duration::from_secs(5),
    )
    .await;
    let late = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        writer.write_all(b"late line\n").await.unwrap();
        drop(writer);
    });

    common::wait_for_log_eof(&path, Duration::from_secs(5))
        .await
        .expect("both consumers reach end of stream");
    // Snapshot the file the instant the waiter returns: what it certified
    // complete is what gets judged, not what drained while we awaited the
    // writer afterwards.
    let text = std::fs::read_to_string(&path).unwrap();
    late.await.unwrap();
    assert!(
        text.contains("late line"),
        "wait_for_log_eof returned before the stream closed, released by a child \
         line that merely quoted the marker:\n{text}"
    );
}
