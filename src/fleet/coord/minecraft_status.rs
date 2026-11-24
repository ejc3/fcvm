//! Minecraft status rows on their way from the hosts' agents to the pages that asked for them
//! (fleet/PROTOCOL.md, "Minecraft status rows"). An agent sends its host's rows about once a
//! second in a health report; the coordinator keeps the last message of each host here, in
//! memory only, and each `/live` connection whose page shows Minecraft mode sends on what it
//! has not sent yet. None of it is part of the fleet's state: it moves no version and is never
//! stored, so a fleet of hundreds of servers costs the store and every other page nothing.

use fleet_core::minecraft::{host_key, MinecraftStatus, STATUS_ROW_BYTES};
use fleet_core::protocol::ServerMsg;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::watch;

/// The most servers one host's rows may describe: a longer report is dropped.
pub const MAX_SERVERS: usize = 4096;

/// The last status message of each host, by its key, with the number of the change that put
/// it there.
#[derive(Debug, Default)]
pub struct StatusSet {
    pub seq: u64,
    pub hosts: BTreeMap<String, (u64, Arc<str>)>,
    /// Each host's current agent connection: the last whose hello the coordinator accepted.
    /// Only it publishes the host's rows and takes them away; a connection it replaced, still
    /// open, does neither.
    owners: BTreeMap<String, u64>,
}

impl StatusSet {
    /// The messages put there after change `sent`.
    pub fn newer(&self, sent: u64) -> impl Iterator<Item = &Arc<str>> {
        self.hosts
            .values()
            .filter(move |(seq, _)| *seq > sent)
            .map(|(_, text)| text)
    }
}

#[derive(Clone)]
pub struct MinecraftStatuses(Arc<watch::Sender<Arc<StatusSet>>>);

impl Default for MinecraftStatuses {
    fn default() -> Self {
        MinecraftStatuses(Arc::new(watch::channel(Arc::new(StatusSet::default())).0))
    }
}

impl MinecraftStatuses {
    /// The coordinator accepted connection `conn`'s hello for `host`: the host's rows are this
    /// connection's from now on, and the rows of the connection it replaced are dropped.
    pub fn claim(&self, host: &str, conn: u64) {
        let key = host_key(host);
        self.0.send_modify(|set| {
            let mut next = StatusSet {
                seq: set.seq + 1,
                hosts: set.hosts.clone(),
                owners: set.owners.clone(),
            };
            next.hosts.remove(&key);
            next.owners.insert(key, conn);
            *set = Arc::new(next);
        });
    }

    /// A host's agent sent its rows over connection `conn`. Dropped unless `conn` is the
    /// host's current connection.
    pub fn publish(&self, host: &str, conn: u64, status: MinecraftStatus) {
        // base64: four characters for three bytes
        if status.rows.len() > MAX_SERVERS * STATUS_ROW_BYTES / 3 * 4 + 4 {
            tracing::warn!(target: "fcvm::fleet::minecraft", %host, chars = status.rows.len(), "dropping a status report with too many rows");
            return;
        }
        let key = host_key(host);
        let text: Arc<str> = serde_json::to_string(&ServerMsg::MinecraftStatus {
            host: key.clone(),
            status,
        })
        .expect("server messages serialize")
        .into();
        self.0.send_if_modified(|set| {
            if set.owners.get(&key) != Some(&conn) {
                return false;
            }
            let mut next = StatusSet {
                seq: set.seq + 1,
                hosts: set.hosts.clone(),
                owners: set.owners.clone(),
            };
            next.hosts.insert(key, (next.seq, text));
            *set = Arc::new(next);
            true
        });
    }

    /// Connection `conn` of a host's agent closed: if it was the host's current one, the
    /// host's rows are no longer sent to a page that asks.
    pub fn forget(&self, host: &str, conn: u64) {
        let key = host_key(host);
        self.0.send_if_modified(|set| {
            if set.owners.get(&key) != Some(&conn) {
                return false;
            }
            let mut next = StatusSet {
                seq: set.seq + 1,
                hosts: set.hosts.clone(),
                owners: set.owners.clone(),
            };
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

    #[test]
    fn a_connection_is_sent_each_hosts_latest_rows_once() {
        let all = MinecraftStatuses::default();
        let mut rx = all.subscribe();
        all.claim("host-a", 1);
        all.claim("host-b", 2);
        all.publish("host-a", 1, status(1, 2));
        all.publish("host-b", 2, status(2, 1));
        all.publish("host-a", 1, status(3, 2));
        let set = rx.borrow_and_update().clone();
        // only the latest of each host, never the host's name
        let texts: Vec<serde_json::Value> = set
            .newer(0)
            .map(|t| serde_json::from_str(t).unwrap())
            .collect();
        assert_eq!(texts.len(), 2);
        let ts: std::collections::BTreeSet<u64> =
            texts.iter().map(|t| t["t"].as_u64().unwrap()).collect();
        assert_eq!(ts, [2, 3].into());
        for t in &texts {
            assert_eq!(t["type"], "minecraft_status");
            assert_eq!(t["host"].as_str().unwrap().len(), 12);
            assert!(!t.to_string().contains("host-a"), "{t}");
        }
        // nothing is sent twice
        assert_eq!(set.newer(set.seq).count(), 0);
        all.publish("host-b", 2, status(4, 1));
        let next = rx.borrow_and_update().clone();
        let again: Vec<&Arc<str>> = next.newer(set.seq).collect();
        assert_eq!(again.len(), 1);
        assert!(again[0].contains("\"t\":4"), "{}", again[0]);
        // a host that left is no longer sent to a page that asks later
        all.forget("host-a", 1);
        assert_eq!(rx.borrow_and_update().newer(0).count(), 1);
        all.forget("host-a", 1);
    }

    #[test]
    fn a_replaced_connection_neither_publishes_nor_takes_away_its_hosts_rows() {
        let all = MinecraftStatuses::default();
        let ts = |all: &MinecraftStatuses| -> Vec<u64> {
            all.subscribe()
                .borrow()
                .newer(0)
                .map(|t| {
                    serde_json::from_str::<serde_json::Value>(t).unwrap()["t"]
                        .as_u64()
                        .unwrap()
                })
                .collect()
        };
        // a connection whose hello was not accepted publishes nothing
        all.publish("host-a", 7, status(1, 1));
        assert!(ts(&all).is_empty());
        all.claim("host-a", 7);
        all.publish("host-a", 7, status(2, 1));
        assert_eq!(ts(&all), [2]);
        // the host dials again while its old connection is still open: the old rows go at once
        all.claim("host-a", 9);
        assert!(ts(&all).is_empty());
        all.publish("host-a", 9, status(3, 1));
        // the replaced connection's late report and its close change nothing
        all.publish("host-a", 7, status(4, 1));
        assert_eq!(ts(&all), [3]);
        all.forget("host-a", 7);
        assert_eq!(ts(&all), [3]);
        // the current connection's close takes the rows away
        all.forget("host-a", 9);
        assert!(ts(&all).is_empty());
    }

    #[test]
    fn a_report_of_more_servers_than_a_host_can_run_is_dropped() {
        let all = MinecraftStatuses::default();
        all.claim("host-a", 1);
        all.claim("host-b", 2);
        all.publish("host-a", 1, status(1, MAX_SERVERS));
        assert_eq!(all.subscribe().borrow().hosts.len(), 1);
        all.publish("host-b", 2, status(1, MAX_SERVERS + 2));
        assert_eq!(all.subscribe().borrow().hosts.len(), 1);
    }
}
