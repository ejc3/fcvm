//! Minecraft mode: Pumpkin servers on fcvm hosts (`docs/fleet/minecraft.md`). Each server is a
//! slot of `Fleet::minecraft` (`mc-<n>`), apart from the wall's slots, the phones and the
//! Windows sandboxes: an fcvm clone of its host's gold snapshot (one server with its world
//! loaded and its bots joined), or a copy of another server's running world (a fork). A viewer
//! borrows one with `claim_minecraft` and drives its first bot over the tile's view socket;
//! no drive input goes through the coordinator.

use crate::tiling::Rect;
use serde::{Deserialize, Serialize};

/// The `id` of the health report that carries a host's status rows.
pub const STATUS_ID: &str = "minecraft";
/// Bytes of one server's status row.
pub const STATUS_ROW_BYTES: usize = 8;
/// With more servers than this the snapshot does not list every server as a tile: it lists
/// the first this many and every lent one, and names them all in `field`.
pub const TILES_LISTED: usize = 16;

/// A host's Minecraft servers as its agent last read them, about once a second
/// (fleet/PROTOCOL.md, "Minecraft status rows").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MinecraftStatus {
    /// When the host read them (Unix ms, the host's clock).
    pub t: u64,
    /// Base64 of [`STATUS_ROW_BYTES`] per server, in the order of the host's servers (their
    /// `index`): [`status_row`].
    pub rows: String,
    /// The host's own readings at `t`: its CPUs, how many were busy over the last second, and
    /// its memory in total and in use (`MemTotal` less `MemAvailable`), in MiB. The whole
    /// host's, whatever else it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub busy: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_total_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_used_mib: Option<u64>,
}

/// One server's status row: its phase (0 not reported, 1 restoring, 2 forking, 3 running,
/// 4 failed), ticks per second times ten (255: none), players (255: none), a zero, then two
/// little-endian 16-bit numbers: seconds since this launch's first chunk and the launch's
/// milliseconds to that chunk (65535: none; larger values are held at 65534).
pub fn status_row(
    phase: &str,
    tps: Option<f64>,
    players: Option<u64>,
    age_s: Option<u64>,
    restore_ms: Option<u64>,
) -> [u8; STATUS_ROW_BYTES] {
    let phase = match phase {
        "restoring" => 1,
        "forking" => 2,
        "running" => 3,
        "failed" => 4,
        _ => 0,
    };
    let tps = match tps {
        Some(t) if t.is_finite() => (t * 10.0).round().clamp(0.0, 250.0) as u8,
        _ => 255,
    };
    let short = |v: Option<u64>| v.map_or(u16::MAX, |v| v.min(65534) as u16).to_le_bytes();
    let (age, restore) = (short(age_s), short(restore_ms));
    [
        phase,
        tps,
        players.map_or(255, |p| p.min(254) as u8),
        0,
        age[0],
        age[1],
        restore[0],
        restore[1],
    ]
}

/// How often an agent reports its host's status rows, ms.
pub const STATUS_EVERY_MS: u64 = 1000;
/// A host's rows older than this say nothing of its servers now: the coordinator sends them to
/// no page, and a page that holds them shows the servers as not reported.
pub const STATUS_EXPIRE_MS: u64 = 5000;

/// Standard base64 with padding, back to bytes; `None` for anything else.
pub fn unbase64(text: &str) -> Option<Vec<u8>> {
    let b = text.as_bytes();
    if !b.len().is_multiple_of(4) {
        return None;
    }
    let val = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let mut out = Vec::with_capacity(b.len() / 4 * 3);
    for (k, q) in b.chunks(4).enumerate() {
        let last = k + 1 == b.len() / 4;
        let pad = q.iter().rev().take_while(|c| **c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for c in &q[..4 - pad] {
            n = n << 6 | val(*c)? as u32;
        }
        n <<= 6 * pad as u32;
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// Standard base64, with padding.
pub fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        out.push(ABC[(n >> 18) as usize & 63] as char);
        out.push(ABC[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ABC[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ABC[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// The name a page knows a host by in Minecraft mode: 12 hex digits of a hash of its name,
/// never the name (which may be an instance id).
pub fn host_key(name: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:012x}", h & 0xffff_ffff_ffff)
}

/// A list of URLs as what they share and what they do not: `prefix + name + suffix` is each
/// URL. A host's servers' view URLs differ in a few characters each.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Squeezed {
    pub prefix: String,
    pub suffix: String,
    /// One per URL, in order; null for a server with no URL now.
    pub names: Vec<Option<String>>,
}

pub fn squeeze(urls: &[Option<String>]) -> Squeezed {
    let some: Vec<&str> = urls.iter().flatten().map(String::as_str).collect();
    let Some(first) = some.first() else {
        return Squeezed {
            prefix: String::new(),
            suffix: String::new(),
            names: vec![None; urls.len()],
        };
    };
    let common = |it: &mut dyn Iterator<Item = (char, char)>| {
        it.take_while(|(a, b)| a == b)
            .map(|(a, _)| a.len_utf8())
            .sum::<usize>()
    };
    let mut pre = first.len();
    let mut suf = first.len();
    for u in &some[1..] {
        pre = pre.min(common(&mut first.chars().zip(u.chars())));
        suf = suf.min(common(&mut first.chars().rev().zip(u.chars().rev())));
    }
    // One URL, or URLs that are all the same: the whole of it is the prefix.
    let shortest = some.iter().map(|u| u.len()).min().unwrap_or(0);
    let suf = suf.min(shortest - pre.min(shortest));
    let pre = pre.min(shortest);
    Squeezed {
        prefix: first[..pre].to_string(),
        suffix: first[first.len() - suf..].to_string(),
        names: urls
            .iter()
            .map(|u| u.as_ref().map(|u| u[pre..u.len() - suf].to_string()))
            .collect(),
    }
}

/// What a server has that a wall slot does not.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct MinecraftSlot {
    /// Its place among its host's servers (0-based, below the hello's `minecraft`): the
    /// agent's address for it.
    pub index: u32,
    pub claim: Option<MinecraftClaim>,
    /// The fork the current generation is, for the agent, until that generation reports
    /// running or the generation moves on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork: Option<PendingFork>,
    /// The server a launch is a copy of, and that launch's generation: shown while the
    /// generation is the server's current one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<ForkedFrom>,
}

/// A viewer's borrowed server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MinecraftClaim {
    pub session: String,
    pub viewer: String,
    pub client: String,
    pub claimed_at: u64,
    pub expires_at: u64,
}

/// A fork asked for and not yet running: the target's generation it made, the source and when
/// and by whom it was clicked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingFork {
    pub generation: u64,
    /// The source's slot (`mc-2`) and its index on the same host.
    pub slot: String,
    pub index: u32,
    pub clicked_at: u64,
    /// `viewer:<id>`.
    pub by: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForkedFrom {
    pub generation: u64,
    pub slot: String,
}

/// The last `minecraft_reset` of every server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResetAll {
    pub clicked_at: u64,
    pub by: String,
    /// The servers it reset.
    pub servers: usize,
    /// How many of them are back: the launch this reset began reported its first chunk. A
    /// server reset again before that, by a viewer or a borrow ending, is never counted, and
    /// neither is a server this reset did not touch. None in a record from before the
    /// coordinator counted; a page then reads the reset from the status rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub back: Option<usize>,
    /// The click to the first chunk of the last of them back, ms. None while none is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub took_ms: Option<u64>,
}

/// The last fork that reached `running`, with the agent's times: `snapshot_ms` the source's
/// `fcvm snapshot create`, `restore_ms` the copy's restore until its view answered, `total_ms`
/// the click to the copy's first chunk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastFork {
    pub from: String,
    pub to: String,
    pub clicked_at: u64,
    pub by: String,
    pub first_chunk_at: Option<u64>,
    pub snapshot_ms: Option<u64>,
    pub restore_ms: Option<u64>,
    pub total_ms: Option<u64>,
}

/// A reset a server is owed because it was lent when its host left: the borrow's session, and
/// once asked for, the generation whose running report settles it (as `windows::ResetOwed`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResetOwed {
    pub session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
}

/// A server's name across connections and coordinator restarts: its host and its index there.
pub fn key(host: &str, index: u32) -> String {
    format!("{host}/{index}")
}

/// The least time between two resets of every server from one client.
pub const RESET_ALL_INTERVAL_MS: u64 = 10_000;
/// The least time between two forks from one client.
pub const FORK_INTERVAL_MS: u64 = 10_000;
/// How long a running server nobody holds keeps its world before it is reset to the gold
/// snapshot. The servers take public joins in offline mode, so a griefed or wedged world lasts
/// at most this long with no viewer to press Reset.
pub const AUTO_RESET_MS: u64 = 30 * 60 * 1000;
/// A server's tile, width / height: the map is 128 x 128 columns.
pub const TILE_ASPECT: f64 = 1.0;

/// The servers' image: each server's rectangle, in order, and the image's width / height. One
/// row of up to four, then rows of equal length, each tile square.
pub fn layout(n: usize) -> (Vec<Rect>, f64) {
    let (rows, cols) = crate::windows::grid(n);
    if n == 0 {
        return (Vec::new(), TILE_ASPECT);
    }
    let rects = (0..n)
        .map(|i| Rect {
            x: (i % cols) as f64 / cols as f64,
            y: (i / cols) as f64 / rows as f64,
            w: 1.0 / cols as f64,
            h: 1.0 / rows as f64,
        })
        .collect();
    (rects, cols as f64 * TILE_ASPECT / rows as f64)
}

/// The name Java Edition players join a server at: the frame URL's host when the URL is
/// `wss://` on a DNS name, and nothing for an IP literal or a plain `ws://` URL.
pub fn join_host(frame_url: &str) -> Option<String> {
    let rest = frame_url.strip_prefix("wss://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') || authority.starts_with('[') {
        return None;
    }
    let host = authority.split(':').next().unwrap_or_default();
    if host.is_empty() || host.parse::<std::net::IpAddr>().is_ok() {
        return None;
    }
    // A DNS name: labels of letters, digits and hyphens.
    let name = host
        .split('.')
        .all(|l| !l.is_empty() && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
    name.then(|| host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers_are_laid_out_in_rows_of_up_to_four_square_tiles() {
        let (rects, aspect) = layout(0);
        assert!(rects.is_empty());
        assert_eq!(aspect, TILE_ASPECT);
        let (rects, aspect) = layout(2);
        assert_eq!(aspect, 2.0);
        assert_eq!(
            rects[1],
            Rect {
                x: 0.5,
                y: 0.0,
                w: 0.5,
                h: 1.0
            }
        );
        let (rects, aspect) = layout(8);
        assert_eq!(aspect, 2.0);
        assert_eq!(
            rects[0],
            Rect {
                x: 0.0,
                y: 0.0,
                w: 0.25,
                h: 0.5
            }
        );
        let (rects, aspect) = layout(5);
        assert_eq!(aspect, 1.5);
        assert_eq!(rects.len(), 5);
        // Each tile is square in the image: its share of the width over its share of the height.
        for r in &rects {
            assert!((r.w * aspect / r.h - 1.0).abs() < 1e-12);
        }
    }

    #[test]
    fn players_join_the_frame_urls_dns_name_and_nothing_else() {
        assert_eq!(
            join_host("wss://mc-1-abcdefgh.fleet.example.org/view").as_deref(),
            Some("mc-1-abcdefgh.fleet.example.org")
        );
        assert_eq!(
            join_host("wss://MC-2.Example.org:8443/view").as_deref(),
            Some("mc-2.example.org")
        );
        for none in [
            "ws://mc-1.example.org/view",
            "wss://192.0.2.7/view",
            "wss://192.0.2.7:443/view",
            "wss://[2001:db8::7]/view",
            "wss://user@mc-1.example.org/view",
            "wss:///view",
            "wss://bad_name.example.org/view",
            "wss://mc-1..example.org/view",
            "https://mc-1.example.org/view",
            "",
        ] {
            assert_eq!(join_host(none), None, "{none:?}");
        }
    }

    #[test]
    fn a_server_slot_round_trips_and_omits_what_it_lacks() {
        let s = MinecraftSlot {
            index: 1,
            ..Default::default()
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v, serde_json::json!({"index": 1, "claim": null}));
        let back: MinecraftSlot = serde_json::from_value(v).unwrap();
        assert_eq!(back, s);
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;

    #[test]
    fn a_status_row_packs_a_servers_state_into_eight_bytes() {
        assert_eq!(
            status_row("running", Some(19.98), Some(3), Some(125), Some(412)),
            [3, 200, 3, 0, 125, 0, 156, 1]
        );
        // nothing read is "none", never zero
        assert_eq!(
            status_row("restoring", None, None, None, None),
            [1, 255, 255, 0, 255, 255, 255, 255]
        );
        assert_eq!(
            status_row("forking", Some(f64::NAN), None, None, None)[..2],
            [2, 255]
        );
        assert_eq!(
            status_row("failed", Some(0.0), Some(0), Some(0), Some(0)),
            [4, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(status_row("pending", None, None, None, None)[0], 0);
        // large values are held below the "none" mark
        assert_eq!(
            status_row(
                "running",
                Some(99.0),
                Some(900),
                Some(1 << 20),
                Some(1 << 20)
            ),
            [3, 250, 254, 0, 254, 255, 254, 255]
        );
    }

    #[test]
    fn base64_is_the_standard_one() {
        for (plain, coded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(plain.as_bytes()), coded);
        }
        assert_eq!(base64(&[0xfb, 0xff, 0xfe]), "+//+");
        // and back: every length, and nothing that is not base64
        for n in 0..40usize {
            let bytes: Vec<u8> = (0..n).map(|i| (i * 37 + n) as u8).collect();
            assert_eq!(unbase64(&base64(&bytes)).as_deref(), Some(&bytes[..]));
        }
        for bad in [
            "A", "AAA", "A===", "=AAA", "AA=A", "AAAA=AAA", "AA*A", "AAAA\n",
        ] {
            assert_eq!(unbase64(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_host_key_is_stable_and_is_not_the_name() {
        let k = host_key("example-host-a");
        assert_eq!(k.len(), 12);
        assert_eq!(k, host_key("example-host-a"));
        assert_ne!(k, host_key("example-host-b"));
        assert!(!k.contains("example"));
    }

    #[test]
    fn squeezed_urls_come_back_whole() {
        let back = |s: &Squeezed| {
            s.names
                .iter()
                .map(|n| n.as_ref().map(|n| format!("{}{n}{}", s.prefix, s.suffix)))
                .collect::<Vec<_>>()
        };
        let urls: Vec<Option<String>> = vec![
            Some("wss://mc-1-abcdefgh.mc.example.test/view".into()),
            None,
            Some("wss://mc-12-zyxwvuts.mc.example.test/view".into()),
        ];
        let s = squeeze(&urls);
        assert_eq!(s.prefix, "wss://mc-1");
        assert_eq!(s.suffix, ".mc.example.test/view");
        assert_eq!(
            s.names,
            vec![Some("-abcdefgh".into()), None, Some("2-zyxwvuts".into())]
        );
        assert_eq!(back(&s), urls);
        // one URL, the same URL twice, a URL that is a prefix of another, and none at all
        for urls in [
            vec![Some("ws://[2001:db8::1]:8080/view".to_string())],
            vec![
                Some("ws://a/view".to_string()),
                Some("ws://a/view".to_string()),
            ],
            vec![
                Some("ws://a/view".to_string()),
                Some("ws://a/view/view".to_string()),
            ],
            vec![None, None],
            vec![],
        ] {
            assert_eq!(back(&squeeze(&urls)), urls, "{urls:?}");
        }
    }
}
