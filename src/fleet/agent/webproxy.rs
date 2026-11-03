//! The phones' web proxy (docs/fleet/ARCHITECTURE.md, "The browser"): the one way a phone's
//! browser reaches the internet. A phone VM is routed over IPv6 only, and www.nvidia.com has
//! no AAAA record, so the browser cannot reach it from the VM. Its Chromium is started with
//! `--proxy-server` naming this proxy: the phone VM forwards the port to the host's loopback
//! (`--forward-localhost`), and the proxy, on the host, opens the connection over the host's
//! own IPv4 or IPv6.
//!
//! It takes HTTP `CONNECT <name>:443` and nothing else, for a name under [`ALLOW`], and
//! connects only to a global unicast address the name resolves to (never the host's loopback,
//! a private network, a link-local address or the instance metadata service). Everything
//! else is answered 403 and closed. The browser speaks TLS to the site through the tunnel,
//! so the proxy sees names and byte counts, never content.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

/// The port the proxy listens on, on the host's loopback, and the port a phone VM forwards
/// (`--forward-localhost`) to it. Part of the phone's snapshot key, so the warm snapshot and
/// the agent agree on it (`the_ami_warm_phone_is_launched_as_a_phone_slot_is`).
pub const PORT: u16 = 3128;

/// The names a phone may open, each with its subdomains: the NVIDIA home page and the
/// hosts its first screen loads from. Third-party hosts (analytics, tag managers, consent
/// and advertising scripts) are left out, and the page draws without them.
pub const ALLOW: &[&str] = &["nvidia.com", "nvidiagrid.net"];

/// The only port a phone may connect to.
const TLS_PORT: u16 = 443;
/// The request head: its size, and how long the client has to send it.
const MAX_HEAD: usize = 4096;
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
/// Connecting to the site, per address.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// A tunnel with no byte either way for this long is closed.
const IDLE: Duration = Duration::from_secs(120);
/// Tunnels open at once; one more is answered 503.
const MAX_TUNNELS: usize = 256;

/// Whether `name` is under one of `ALLOW`'s names: the name itself, or a subdomain of it.
pub fn allowed(name: &str) -> bool {
    ALLOW
        .iter()
        .any(|a| name == *a || name.strip_suffix(a).is_some_and(|p| p.ends_with('.')))
}

/// The tunnel a request head asks for: `CONNECT <name>:443 HTTP/1.x`, the name a DNS name
/// under [`ALLOW`] (lowercased; no address literal). Any other request is refused with the
/// reason.
pub fn parse_connect(head: &[u8]) -> Result<String, String> {
    let line = head
        .split(|b| *b == b'\n')
        .next()
        .unwrap_or_default()
        .strip_suffix(b"\r")
        .ok_or("the request line does not end in CRLF")?;
    let line = std::str::from_utf8(line).map_err(|_| "the request line is not UTF-8")?;
    let mut parts = line.split(' ');
    let (method, target, version) = (parts.next(), parts.next(), parts.next());
    if parts.next().is_some() || !matches!(version, Some("HTTP/1.1" | "HTTP/1.0")) {
        return Err(format!("not an HTTP/1 request line: {line:?}"));
    }
    if method != Some("CONNECT") {
        return Err(format!(
            "only CONNECT is served, not {}",
            method.unwrap_or("")
        ));
    }
    let target = target.unwrap_or_default();
    let (name, port) = target
        .rsplit_once(':')
        .ok_or_else(|| format!("{target:?} names no port"))?;
    if port != TLS_PORT.to_string() {
        return Err(format!("only port {TLS_PORT} is served, not {port:?}"));
    }
    let name = name.to_ascii_lowercase();
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    if name.len() > 253 || !name.split('.').all(label_ok) {
        return Err(format!("{name:?} is not a DNS name"));
    }
    if !allowed(&name) {
        return Err(format!("{name} is not on the phone's allow list"));
    }
    Ok(name)
}

/// An address the proxy may connect to: global unicast, not this host or its networks.
pub fn global(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => v4_global(a),
        IpAddr::V6(a) => match a.to_ipv4_mapped() {
            Some(v4) => v4_global(v4),
            None => v6_global(a),
        },
    }
}

fn v4_global(a: Ipv4Addr) -> bool {
    let [o0, o1, ..] = a.octets();
    !(a.is_unspecified()
        || a.is_loopback()
        || a.is_private()
        || a.is_link_local()
        || a.is_broadcast()
        || a.is_multicast()
        || a.is_documentation()
        || o0 == 0
        // shared address space (carrier NAT, 100.64.0.0/10) and 192.0.0.0/24
        || (o0 == 100 && (o1 & 0xc0) == 64)
        || (o0 == 192 && o1 == 0 && a.octets()[2] == 0)
        // benchmarking, and reserved
        || (o0 == 198 && (o1 & 0xfe) == 18)
        || o0 >= 240)
}

fn v6_global(a: Ipv6Addr) -> bool {
    let s = a.segments();
    // 2000::/3, without documentation (2001:db8::/32), and not the AWS metadata or resolver
    // addresses, which are in fd00:ec2::/32 (unique local, outside 2000::/3 already)
    (s[0] & 0xe000) == 0x2000 && !(s[0] == 0x2001 && s[1] == 0x0db8)
}

/// Where a tunnel to an allowed name connects.
#[derive(Clone, Copy, Debug)]
enum Upstream {
    /// The name's global addresses, through the host's resolver.
    Dns,
    /// One address, whatever the name (tests: a local server stands in for the site).
    #[cfg(test)]
    Fixed(SocketAddr),
}

/// Serve the proxy on `listener` until the task is dropped.
pub async fn serve(listener: TcpListener) {
    serve_to(listener, Upstream::Dns).await
}

async fn serve_to(listener: TcpListener, upstream: Upstream) {
    let slots = Arc::new(Semaphore::new(MAX_TUNNELS));
    loop {
        let (client, peer) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "phone web proxy: accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let slot = slots.clone().try_acquire_owned();
        tokio::spawn(async move {
            let Ok(_slot) = slot else {
                let mut client = client;
                let _ = client
                    .write_all(b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\n\r\n")
                    .await;
                return;
            };
            if let Err(e) = tunnel(client, upstream).await {
                tracing::debug!(%peer, error = %e, "phone web proxy: tunnel ended");
            }
        });
    }
}

/// Listen on `addr` and serve; the listener is bound before this returns.
pub async fn spawn(addr: SocketAddr) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("binding the phone web proxy on {addr}: {e}"))?;
    tracing::info!(%addr, allow = ?ALLOW, "phone web proxy listening");
    Ok(tokio::spawn(serve(listener)))
}

async fn read_head(client: &mut TcpStream) -> anyhow::Result<Vec<u8>> {
    let mut head = Vec::with_capacity(512);
    let mut buf = [0u8; 512];
    let read = async {
        loop {
            if head.windows(4).any(|w| w == b"\r\n\r\n") {
                return Ok(());
            }
            if head.len() >= MAX_HEAD {
                anyhow::bail!("the request head is longer than {MAX_HEAD} bytes");
            }
            // one byte past the head would be the client's TLS, which this never reads
            let want = (MAX_HEAD - head.len()).min(buf.len());
            let n = client.read(&mut buf[..want]).await?;
            if n == 0 {
                anyhow::bail!("the client closed before the request head ended");
            }
            head.extend_from_slice(&buf[..n]);
        }
    };
    tokio::time::timeout(HEAD_TIMEOUT, read)
        .await
        .map_err(|_| anyhow::anyhow!("no request head within {HEAD_TIMEOUT:?}"))??;
    Ok(head)
}

async fn tunnel(mut client: TcpStream, to: Upstream) -> anyhow::Result<()> {
    let head = read_head(&mut client).await?;
    let end = head
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .unwrap_or(head.len());
    let name = match parse_connect(&head[..end]) {
        Ok(n) => n,
        Err(why) => {
            tracing::info!(reason = %why, "phone web proxy: refused");
            client
                .write_all(b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n")
                .await?;
            return Ok(());
        }
    };
    let opened = match to {
        Upstream::Dns => open(&name).await,
        #[cfg(test)]
        Upstream::Fixed(a) => TcpStream::connect(a).await.map_err(Into::into),
    };
    let upstream = match opened {
        Ok(u) => u,
        Err(e) => {
            tracing::info!(%name, error = %e, "phone web proxy: cannot reach");
            client
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n")
                .await?;
            return Ok(());
        }
    };
    client
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .await?;
    // bytes the client sent after its head (it may start TLS without waiting)
    let mut upstream = upstream;
    if end < head.len() {
        upstream.write_all(&head[end..]).await?;
    }
    let (up, down) = relay(client, upstream).await;
    tracing::debug!(%name, up, down, "phone web proxy: tunnel closed");
    Ok(())
}

/// A connection to `name`:443 on the first global address it resolves to that answers.
async fn open(name: &str) -> anyhow::Result<TcpStream> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((name, TLS_PORT)).await?.collect();
    let mut last = anyhow::anyhow!("{name} resolves to no address");
    for a in addrs {
        if !global(a.ip()) {
            last = anyhow::anyhow!(
                "{name} resolves to {}, which is not a global address",
                a.ip()
            );
            continue;
        }
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(a)).await {
            Ok(Ok(s)) => {
                s.set_nodelay(true).ok();
                return Ok(s);
            }
            Ok(Err(e)) => last = anyhow::anyhow!("{a}: {e}"),
            Err(_) => last = anyhow::anyhow!("{a}: no answer within {CONNECT_TIMEOUT:?}"),
        }
    }
    Err(last)
}

/// Copy both ways until either side closes or neither sends for `IDLE`; the bytes each way.
async fn relay(a: TcpStream, b: TcpStream) -> (u64, u64) {
    let (mut ar, mut aw) = a.into_split();
    let (mut br, mut bw) = b.into_split();
    let (mut up, mut down) = (0u64, 0u64);
    let mut abuf = vec![0u8; 32 * 1024];
    let mut bbuf = vec![0u8; 32 * 1024];
    loop {
        tokio::select! {
            r = tokio::time::timeout(IDLE, ar.read(&mut abuf)) => match r {
                Ok(Ok(n)) if n > 0 => {
                    if bw.write_all(&abuf[..n]).await.is_err() { break; }
                    up += n as u64;
                }
                _ => break,
            },
            r = tokio::time::timeout(IDLE, br.read(&mut bbuf)) => match r {
                Ok(Ok(n)) if n > 0 => {
                    if aw.write_all(&bbuf[..n]).await.is_err() { break; }
                    down += n as u64;
                }
                _ => break,
            },
        }
    }
    let _ = aw.shutdown().await;
    let _ = bw.shutdown().await;
    (up, down)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_connect_to_an_allowed_name_on_443_is_served() {
        let ok = |h: &str| parse_connect(h.as_bytes());
        assert_eq!(
            ok("CONNECT www.nvidia.com:443 HTTP/1.1\r\nHost: www.nvidia.com:443\r\n\r\n").unwrap(),
            "www.nvidia.com"
        );
        assert_eq!(
            ok("CONNECT NVIDIA.com:443 HTTP/1.0\r\n\r\n").unwrap(),
            "nvidia.com"
        );
        assert_eq!(
            ok("CONNECT brand-assets.cne.ngc.nvidia.com:443 HTTP/1.1\r\n\r\n").unwrap(),
            "brand-assets.cne.ngc.nvidia.com"
        );
        for refused in [
            // not on the list: the page's third-party scripts, and the browser's own updates
            "CONNECT cdn.cookielaw.org:443 HTTP/1.1\r\n\r\n",
            "CONNECT assets.adobedtm.com:443 HTTP/1.1\r\n\r\n",
            "CONNECT uazo.github.io:443 HTTP/1.1\r\n\r\n",
            // a suffix that is not a label boundary, and a name that only contains it
            "CONNECT evilnvidia.com:443 HTTP/1.1\r\n\r\n",
            "CONNECT nvidia.com.example.org:443 HTTP/1.1\r\n\r\n",
            // another port, an address literal, plain HTTP, a malformed line
            "CONNECT www.nvidia.com:80 HTTP/1.1\r\n\r\n",
            "CONNECT 169.254.169.254:443 HTTP/1.1\r\n\r\n",
            "CONNECT [::1]:443 HTTP/1.1\r\n\r\n",
            "GET http://www.nvidia.com/ HTTP/1.1\r\n\r\n",
            "CONNECT www.nvidia.com:443\r\n\r\n",
            "CONNECT www.nvidia.com:443 HTTP/1.1 x\r\n\r\n",
            "CONNECT www.nvidia.com:443 HTTP/1.1\n\n",
            "CONNECT -x.nvidia.com:443 HTTP/1.1\r\n\r\n",
            "CONNECT a..nvidia.com:443 HTTP/1.1\r\n\r\n",
        ] {
            assert!(ok(refused).is_err(), "{refused:?} was served");
        }
    }

    #[test]
    fn only_global_unicast_addresses_are_connected_to() {
        for a in [
            "23.212.62.203",
            "34.194.97.138",
            "2600:1406:3a00:a::17d8:95cc",
            "::ffff:23.212.62.203",
        ] {
            assert!(global(a.parse().unwrap()), "{a}");
        }
        for a in [
            "127.0.0.1",
            "10.0.2.2",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "198.18.0.1",
            "192.0.0.8",
            "240.0.0.1",
            "192.0.2.1",
            "::1",
            "::",
            "fd00:ec2::254",
            "fe80::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "ff02::1",
        ] {
            assert!(!global(a.parse().unwrap()), "{a}");
        }
    }

    async fn proxy_to(site: SocketAddr) -> SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        tokio::spawn(serve_to(l, Upstream::Fixed(site)));
        a
    }

    async fn reply_line(c: &mut TcpStream) -> String {
        let mut got = Vec::new();
        let mut b = [0u8; 1];
        while !got.ends_with(b"\r\n\r\n") {
            if c.read(&mut b).await.unwrap() == 0 {
                break;
            }
            got.push(b[0]);
        }
        String::from_utf8_lossy(&got)
            .lines()
            .next()
            .unwrap_or("")
            .to_string()
    }

    #[tokio::test]
    async fn an_allowed_tunnel_carries_bytes_both_ways_and_a_refused_one_reaches_nothing() {
        // the site: echoes what it reads, upper-cased, and counts its connections
        let site = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let site_addr = site.local_addr().unwrap();
        let (seen_tx, mut seen) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = site.accept().await.unwrap();
                seen_tx.send(()).unwrap();
                tokio::spawn(async move {
                    let mut b = [0u8; 64];
                    while let Ok(n) = s.read(&mut b).await {
                        if n == 0 {
                            break;
                        }
                        s.write_all(&b[..n].to_ascii_uppercase()).await.unwrap();
                    }
                });
            }
        });
        let proxy = proxy_to(site_addr).await;

        let mut c = TcpStream::connect(proxy).await.unwrap();
        // the client's first bytes arrive with its head, as a browser may send them
        c.write_all(b"CONNECT www.nvidia.com:443 HTTP/1.1\r\nHost: www.nvidia.com\r\n\r\nhello")
            .await
            .unwrap();
        assert_eq!(
            reply_line(&mut c).await,
            "HTTP/1.1 200 Connection established"
        );
        let mut b = [0u8; 5];
        c.read_exact(&mut b).await.unwrap();
        assert_eq!(&b, b"HELLO");
        c.write_all(b"again").await.unwrap();
        c.read_exact(&mut b).await.unwrap();
        assert_eq!(&b, b"AGAIN");
        assert!(seen.recv().await.is_some());

        for head in [
            "CONNECT cdn.cookielaw.org:443 HTTP/1.1\r\n\r\n",
            "GET http://www.nvidia.com/ HTTP/1.1\r\n\r\n",
            "CONNECT www.nvidia.com:22 HTTP/1.1\r\n\r\n",
        ] {
            let mut c = TcpStream::connect(proxy).await.unwrap();
            c.write_all(head.as_bytes()).await.unwrap();
            assert_eq!(
                reply_line(&mut c).await,
                "HTTP/1.1 403 Forbidden",
                "{head:?}"
            );
            let mut rest = Vec::new();
            c.read_to_end(&mut rest).await.unwrap();
            assert!(
                rest.is_empty(),
                "{head:?}: the proxy sent more after refusing"
            );
        }
        // none of the refused requests reached the site
        assert!(seen.try_recv().is_err());

        // a head that never ends is cut off at MAX_HEAD
        let mut c = TcpStream::connect(proxy).await.unwrap();
        let long = format!(
            "CONNECT www.nvidia.com:443 HTTP/1.1\r\nX: {}",
            "a".repeat(MAX_HEAD)
        );
        let _ = c.write_all(long.as_bytes()).await;
        let mut rest = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), c.read_to_end(&mut rest))
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&rest).contains("200"), "{rest:?}");
        assert!(seen.try_recv().is_err());
    }
}
