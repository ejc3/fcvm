//! The workload every boot-race lane starts. One static binary, std only.
//!
//! - HTTP mode (the default): listens on 0.0.0.0:$PORT (8080) and answers every request with JSON.
//! - Lambda mode (AWS_LAMBDA_RUNTIME_API is set, so this binary is the custom runtime's `bootstrap`): the runtime
//!   loop. With BOOT_RACE_EXIT_AFTER_RESPONSE=1 the process exits once Lambda has accepted its response, so the next
//!   request needs a new runtime process.
//!
//! The answer is the count of primes below 200,000 (a fixed CPU job of about a millisecond) and three identifiers: a
//! random id drawn at process start (`proc_id`), the kernel's boot id (`boot_id`) and the process start time. A lane
//! that claims a fresh environment per request shows a new `proc_id`, and a new `boot_id` if the kernel is new too.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LIMIT: usize = 200_000;
const MAX_REQUEST_HEAD: usize = 16 * 1024;

struct Ident {
    proc_id: String,
    boot_id: String,
    started_unix_ms: u128,
    served: AtomicU64,
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn random_hex() -> String {
    let mut b = [0u8; 16];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .is_ok();
    if !ok {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        b.copy_from_slice(&n.to_le_bytes());
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

fn primes_below(n: usize) -> usize {
    let mut composite = vec![false; n];
    let mut count = 0;
    for i in 2..n {
        if !composite[i] {
            count += 1;
            let mut j = i * i;
            while j < n {
                composite[j] = true;
                j += i;
            }
        }
    }
    count
}

fn answer(id: &Ident) -> String {
    let served = id.served.fetch_add(1, Ordering::Relaxed) + 1;
    let t = Instant::now();
    let primes = primes_below(LIMIT);
    let work_us = t.elapsed().as_micros();
    format!(
        concat!(
            r#"{{"ok":true,"primes_below_200000":{},"work_us":{},"proc_id":"{}","boot_id":"{}","#,
            r#""started_unix_ms":{},"answered_unix_ms":{},"served":{}}}"#
        ),
        primes,
        work_us,
        id.proc_id,
        id.boot_id,
        id.started_unix_ms,
        now_unix_ms(),
        served
    )
}

fn serve_http(id: Arc<Ident>) -> io::Result<()> {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let listener = TcpListener::bind(("0.0.0.0", port))?;
    eprintln!(
        "boot-race-workload listening on 0.0.0.0:{port} proc_id={} boot_id={}",
        id.proc_id, id.boot_id
    );
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let id = Arc::clone(&id);
        std::thread::spawn(move || {
            let _ = handle(stream, &id);
        });
    }
    Ok(())
}

fn handle(mut stream: TcpStream, id: &Ident) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut head = Vec::with_capacity(1024);
    let mut buf = [0u8; 2048];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        head.extend_from_slice(&buf[..n]);
        if head.len() > MAX_REQUEST_HEAD {
            return Ok(());
        }
    }
    let body = answer(id);
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(resp.as_bytes())?;
    stream.flush()
}

/// One request to the Lambda Runtime API (plain HTTP on the sandbox's loopback), returning headers and body.
fn runtime_call(api: &str, method: &str, path: &str, body: &str) -> io::Result<(String, String)> {
    let mut s = TcpStream::connect(api)?;
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {api}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes())?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    match text.split_once("\r\n\r\n") {
        Some((h, b)) => Ok((h.to_string(), b.to_string())),
        None => Err(io::Error::other("runtime API answer without a header end")),
    }
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn serve_lambda(api: &str, id: &Ident) -> io::Result<()> {
    let exit_after = std::env::var("BOOT_RACE_EXIT_AFTER_RESPONSE").as_deref() == Ok("1");
    eprintln!(
        "boot-race-workload lambda runtime proc_id={} boot_id={} exit_after_response={exit_after}",
        id.proc_id, id.boot_id
    );
    loop {
        let (head, _event) = runtime_call(api, "GET", "/2018-06-01/runtime/invocation/next", "")?;
        let Some(request_id) = header(&head, "Lambda-Runtime-Aws-Request-Id").map(str::to_string) else {
            return Err(io::Error::other("next invocation without a request id"));
        };
        let body = answer(id);
        let payload = format!(
            r#"{{"statusCode":200,"headers":{{"content-type":"application/json"}},"body":{}}}"#,
            json_string(&body)
        );
        let path = format!("/2018-06-01/runtime/invocation/{request_id}/response");
        let (rhead, _) = runtime_call(api, "POST", &path, &payload)?;
        if exit_after {
            // The status line of the Runtime API's answer (202 when it took the response).
            eprintln!(
                "boot-race-workload exiting after response: {}",
                rhead.lines().next().unwrap_or("")
            );
            std::process::exit(0);
        }
    }
}

fn main() {
    let id = Arc::new(Ident {
        proc_id: random_hex(),
        boot_id: boot_id(),
        started_unix_ms: now_unix_ms(),
        served: AtomicU64::new(0),
    });
    let result = match std::env::var("AWS_LAMBDA_RUNTIME_API") {
        Ok(api) => serve_lambda(&api, &id),
        Err(_) => serve_http(Arc::clone(&id)),
    };
    if let Err(e) = result {
        eprintln!("boot-race-workload: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prime_count_is_the_known_value() {
        assert_eq!(primes_below(LIMIT), 17_984);
    }

    #[test]
    fn json_string_escapes_quotes() {
        assert_eq!(json_string(r#"{"a":"b"}"#), r#""{\"a\":\"b\"}""#);
    }

    #[test]
    fn header_lookup_ignores_case() {
        let h = "HTTP/1.1 200 OK\r\nlambda-runtime-aws-request-id: abc\r\n";
        assert_eq!(header(h, "Lambda-Runtime-Aws-Request-Id"), Some("abc"));
    }
}
