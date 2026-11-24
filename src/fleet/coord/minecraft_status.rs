//! Minecraft status rows on their way from the hosts' agents to the pages that asked for them
//! (fleet/PROTOCOL.md, "Minecraft status rows"). An agent sends its host's rows about once a
//! second in a health report; the coordinator keeps the last message of each host here, in
//! memory only, and each `/live` connection whose page shows Minecraft mode sends on what it
//! has not sent yet. None of it is part of the fleet's state: it moves no version and is never
//! stored, so a fleet of hundreds of servers costs the store and every other page nothing.
//!
//! Rows are a reading, not a state: a host that stops sending leaves its servers unreported.
//! Each host's rows carry when they came, and past [`STATUS_EXPIRE_MS`] they are sent to no
//! page. A report is taken only from the host's current connection, no more often than twice
//! a period, and only when its rows are whole rows for no more servers than the host said it
//! runs.

use fleet_core::minecraft::{
    host_key, unbase64, MinecraftStatus, STATUS_EVERY_MS, STATUS_EXPIRE_MS, STATUS_ROW_BYTES,
};
use fleet_core::protocol::ServerMsg;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// The most servers one host's rows may describe, whatever its hello said.
pub const MAX_SERVERS: usize = 4096;
/// A connection's reports closer together than this are dropped: half the period an agent
/// reports at.
pub const MIN_GAP: Duration = Duration::from_millis(STATUS_EVERY_MS / 2);
const EXPIRE: Duration = Duration::from_millis(STATUS_EXPIRE_MS);

/// A host's current agent connection: the last whose hello the coordinator accepted. Only it
/// publishes the host's rows and takes them away; a connection it replaced, still open, does
/// neither.
#[derive(Debug, Clone)]
struct Owner {
    conn: u64,
    /// The Minecraft servers the host's hello said it runs: its rows describe no more.
    servers: u32,
    /// When its last report was taken.
    last: Option<Instant>,
}

#[derive(Debug, Clone)]
struct Rows {
    seq: u64,
    text: Arc<str>,
    received: Instant,
}

/// The last status message of each host, by its key, with the number of the change that put
/// it there and when it came.
#[derive(Debug, Default)]
pub struct StatusSet {
    pub seq: u64,
    hosts: BTreeMap<String, Rows>,
    owners: BTreeMap<String, Owner>,
}

impl StatusSet {
    /// The messages put there after change `sent` that are not too old at `now` to say
    /// anything of their servers.
    pub fn newer(&self, sent: u64, now: Instant) -> impl Iterator<Item = &Arc<str>> {
        self.hosts
            .values()
            .filter(move |r| r.seq > sent && now.saturating_duration_since(r.received) <= EXPIRE)
            .map(|r| &r.text)
    }

    /// Hosts with rows, expired or not.
    pub fn len(&self) -> usize {
        self.hosts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }

    fn next(&self) -> StatusSet {
        StatusSet {
            seq: self.seq + 1,
            hosts: self.hosts.clone(),
            owners: self.owners.clone(),
        }
    }
}

#[derive(Clone)]
pub struct MinecraftStatuses(Arc<watch::Sender<Arc<StatusSet>>>);

impl Default for MinecraftStatuses {
    fn default() -> Self {
        MinecraftStatuses(Arc::new(watch::channel(Arc::new(StatusSet::default())).0))
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

impl MinecraftStatuses {
    /// The coordinator accepted connection `conn`'s hello for `host`, which said it runs
    /// `servers` Minecraft servers: the host's rows are this connection's from now on, and the
    /// rows of the connection it replaced are dropped.
    pub fn claim(&self, host: &str, conn: u64, servers: u32) {
        let key = host_key(host);
        self.0.send_modify(|set| {
            let mut next = set.next();
            next.hosts.remove(&key);
            next.owners.insert(
                key,
                Owner {
                    conn,
                    servers,
                    last: None,
                },
            );
            *set = Arc::new(next);
        });
    }

    /// A host's agent sent its rows over connection `conn`.
    pub fn publish(&self, host: &str, conn: u64, status: MinecraftStatus) {
        self.publish_at(host, conn, status, Instant::now(), unix_ms());
    }

    /// [`publish`](Self::publish) at `now` (`received_at` on the coordinator's clock). Dropped
    /// unless `conn` is the host's current connection, its last report is at least
    /// [`MIN_GAP`] old, and `rows` is base64 of whole rows for no more servers than the host
    /// runs. Whether it was taken.
    pub fn publish_at(
        &self,
        host: &str,
        conn: u64,
        status: MinecraftStatus,
        now: Instant,
        received_at: u64,
    ) -> bool {
        let key = host_key(host);
        let rows = unbase64(&status.rows);
        self.0.send_if_modified(|set| {
            let Some(owner) = set.owners.get(&key).filter(|o| o.conn == conn) else {
                return false;
            };
            if owner
                .last
                .is_some_and(|last| now.saturating_duration_since(last) < MIN_GAP)
            {
                return false;
            }
            let most = (owner.servers as usize).min(MAX_SERVERS);
            let whole = rows
                .as_ref()
                .is_some_and(|r| r.len().is_multiple_of(STATUS_ROW_BYTES) && r.len() / STATUS_ROW_BYTES <= most);
            if !whole {
                tracing::warn!(target: "fcvm::fleet::minecraft", %host, chars = status.rows.len(), servers = owner.servers, "dropping a status report whose rows are not whole rows for the host's servers");
                return false;
            }
            let mut next = set.next();
            let text: Arc<str> = serde_json::to_string(&ServerMsg::MinecraftStatus {
                host: key.clone(),
                received_at,
                status: status.clone(),
            })
            .expect("server messages serialize")
            .into();
            // rows that expired say nothing: they go when anything is taken
            next.hosts
                .retain(|_, r| now.saturating_duration_since(r.received) <= EXPIRE);
            next.hosts.insert(
                key.clone(),
                Rows {
                    seq: next.seq,
                    text,
                    received: now,
                },
            );
            if let Some(o) = next.owners.get_mut(&key) {
                o.last = Some(now);
            }
            *set = Arc::new(next);
            true
        })
    }

    /// Connection `conn` of a host's agent closed: if it was the host's current one, the
    /// host's rows are no longer sent to a page that asks.
    pub fn forget(&self, host: &str, conn: u64) {
        let key = host_key(host);
        self.0.send_if_modified(|set| {
            if set.owners.get(&key).is_none_or(|o| o.conn != conn) {
                return false;
            }
            let mut next = set.next();
            next.hosts.remove(&key);
            next.owners.remove(&key);
            *set = Arc::new(next);
            true
        });
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<StatusSet>> {
        self.0.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(t: u64, servers: usize) -> MinecraftStatus {
        MinecraftStatus {
            t,
            rows: fleet_core::minecraft::base64(&vec![3u8; servers * STATUS_ROW_BYTES]),
            cpus: None,
            busy: None,
            mem_total_mib: None,
            mem_used_mib: None,
        }
    }

    /// The `t` of every message a page that asks at `now` is sent.
    fn ts(all: &MinecraftStatuses, now: Instant) -> Vec<u64> {
        all.subscribe()
            .borrow()
            .newer(0, now)
            .map(|t| {
                serde_json::from_str::<serde_json::Value>(t).unwrap()["t"]
                    .as_u64()
                    .unwrap()
            })
            .collect()
    }

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn a_connection_is_sent_each_hosts_latest_rows_once() {
        let all = MinecraftStatuses::default();
        let mut rx = all.subscribe();
        let t0 = Instant::now();
        all.claim("host-a", 1, 2);
        all.claim("host-b", 2, 1);
        assert!(all.publish_at("host-a", 1, status(1, 2), t0, 7_000));
        assert!(all.publish_at("host-b", 2, status(2, 1), t0, 7_000));
        assert!(all.publish_at("host-a", 1, status(3, 2), t0 + S, 8_000));
        let set = rx.borrow_and_update().clone();
        // only the latest of each host, never the host's name, each with when it came
        let texts: Vec<serde_json::Value> = set
            .newer(0, t0 + S)
            .map(|t| serde_json::from_str(t).unwrap())
            .collect();
        assert_eq!(texts.len(), 2);
        let seen: std::collections::BTreeSet<(u64, u64)> = texts
            .iter()
            .map(|t| (t["t"].as_u64().unwrap(), t["received_at"].as_u64().unwrap()))
            .collect();
        assert_eq!(seen, [(2, 7_000), (3, 8_000)].into());
        for t in &texts {
            assert_eq!(t["type"], "minecraft_status");
            assert_eq!(t["host"].as_str().unwrap().len(), 12);
            assert!(!t.to_string().contains("host-a"), "{t}");
        }
        // nothing is sent twice
        assert_eq!(set.newer(set.seq, t0 + S).count(), 0);
        assert!(all.publish_at("host-b", 2, status(4, 1), t0 + S, 8_000));
        let next = rx.borrow_and_update().clone();
        let again: Vec<&Arc<str>> = next.newer(set.seq, t0 + S).collect();
        assert_eq!(again.len(), 1);
        assert!(again[0].contains("\"t\":4"), "{}", again[0]);
        // a host that left is no longer sent to a page that asks later
        all.forget("host-a", 1);
        assert_eq!(rx.borrow_and_update().newer(0, t0 + S).count(), 1);
        all.forget("host-a", 1);
    }

    /// A host whose connection stays up but which stops sending (its board emptied, or its
    /// status task died) must not go on being shown as it last was.
    #[test]
    fn rows_a_host_stopped_sending_are_sent_to_no_page() {
        let all = MinecraftStatuses::default();
        let t0 = Instant::now();
        all.claim("host-a", 1, 2);
        all.claim("host-b", 2, 2);
        assert!(all.publish_at("host-a", 1, status(1, 2), t0, 0));
        assert!(all.publish_at("host-b", 2, status(2, 2), t0, 0));
        // a few periods missed is not yet silence
        assert_eq!(ts(&all, t0 + 4 * S).len(), 2);
        assert_eq!(ts(&all, t0 + EXPIRE).len(), 2);
        // past that, a page that asks is sent nothing of either host
        assert!(ts(&all, t0 + EXPIRE + Duration::from_millis(1)).is_empty());
        // host-b goes on reporting: only it is sent, and host-a's old rows are gone
        assert!(all.publish_at("host-b", 2, status(3, 2), t0 + 6 * S, 0));
        assert_eq!(ts(&all, t0 + 6 * S), [3]);
        assert_eq!(all.subscribe().borrow().len(), 1);
        // and host-a is sent again as soon as it reports again
        assert!(all.publish_at("host-a", 1, status(4, 2), t0 + 7 * S, 0));
        let mut both = ts(&all, t0 + 7 * S);
        both.sort();
        assert_eq!(both, [3, 4]);
    }

    #[test]
    fn a_replaced_connection_neither_publishes_nor_takes_away_its_hosts_rows() {
        let all = MinecraftStatuses::default();
        let t0 = Instant::now();
        // a connection whose hello was not accepted publishes nothing
        assert!(!all.publish_at("host-a", 7, status(1, 1), t0, 0));
        assert!(ts(&all, t0).is_empty());
        all.claim("host-a", 7, 1);
        assert!(all.publish_at("host-a", 7, status(2, 1), t0, 0));
        assert_eq!(ts(&all, t0), [2]);
        // the host dials again while its old connection is still open: the old rows go at once
        all.claim("host-a", 9, 1);
        assert!(ts(&all, t0).is_empty());
        assert!(all.publish_at("host-a", 9, status(3, 1), t0, 0));
        // the replaced connection's late report and its close change nothing
        assert!(!all.publish_at("host-a", 7, status(4, 1), t0 + S, 0));
        assert_eq!(ts(&all, t0 + S), [3]);
        all.forget("host-a", 7);
        assert_eq!(ts(&all, t0 + S), [3]);
        // the current connection's close takes the rows away
        all.forget("host-a", 9);
        assert!(ts(&all, t0 + S).is_empty());
    }

    #[test]
    fn a_connection_is_heard_no_more_than_twice_a_period() {
        let all = MinecraftStatuses::default();
        let t0 = Instant::now();
        all.claim("host-a", 1, 1);
        assert!(all.publish_at("host-a", 1, status(1, 1), t0, 0));
        // a flood: everything inside the gap is dropped, and does not push the gap out
        for ms in [1, 100, 499] {
            assert!(!all.publish_at("host-a", 1, status(9, 1), t0 + Duration::from_millis(ms), 0));
        }
        assert_eq!(ts(&all, t0 + MIN_GAP), [1]);
        assert!(all.publish_at("host-a", 1, status(2, 1), t0 + MIN_GAP, 0));
        // an agent's own pace, a period apart, is all taken
        assert!(all.publish_at("host-a", 1, status(3, 1), t0 + MIN_GAP + S, 0));
        assert_eq!(ts(&all, t0 + MIN_GAP + S), [3]);
    }

    #[test]
    fn rows_that_are_not_whole_rows_for_the_hosts_servers_are_dropped() {
        let all = MinecraftStatuses::default();
        let t0 = Instant::now();
        all.claim("host-a", 1, 2);
        let with = |rows: String| MinecraftStatus {
            rows,
            ..status(5, 0)
        };
        // not base64, part of a row, and a row more than the host said it runs
        assert!(!all.publish_at("host-a", 1, with("!!!!".into()), t0, 0));
        assert!(!all.publish_at(
            "host-a",
            1,
            with(fleet_core::minecraft::base64(&[3u8; 12])),
            t0,
            0
        ));
        assert!(!all.publish_at("host-a", 1, status(5, 3), t0, 0));
        assert!(ts(&all, t0).is_empty());
        // a dropped report does not start the gap: the next good one is taken at once
        assert!(all.publish_at("host-a", 1, status(6, 2), t0, 0));
        // fewer rows than servers (a host that runs fewer just now), and none at all, are whole
        assert!(all.publish_at("host-a", 1, status(7, 1), t0 + S, 0));
        assert!(all.publish_at("host-a", 1, status(8, 0), t0 + 2 * S, 0));
        assert_eq!(ts(&all, t0 + 2 * S), [8]);
        // no hello can raise the bound past MAX_SERVERS
        all.claim("host-b", 2, u32::MAX);
        assert!(all.publish_at("host-b", 2, status(1, MAX_SERVERS), t0, 0));
        assert!(!all.publish_at("host-b", 2, status(2, MAX_SERVERS + 1), t0 + S, 0));
    }
}
