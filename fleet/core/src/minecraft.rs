//! Minecraft mode: Pumpkin servers on fcvm hosts (`docs/fleet/minecraft.md`). Each server is a
//! slot of `Fleet::minecraft` (`mc-<n>`), apart from the wall's slots, the phones and the
//! Windows sandboxes: an fcvm clone of its host's gold snapshot (one server with its world
//! loaded and its bots joined), or a copy of another server's running world (a fork). A viewer
//! borrows one with `claim_minecraft` and drives its first bot over the tile's view socket;
//! no drive input goes through the coordinator.

use crate::tiling::Rect;
use serde::{Deserialize, Serialize};

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
