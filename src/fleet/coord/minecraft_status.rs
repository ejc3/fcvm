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
    /// A host's agent sent its rows.
    pub fn publish(&self, host: &str, status: MinecraftStatus) {
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
        self.0.send_modify(|set| {
            let mut next = StatusSet {
                seq: set.seq + 1,
                hosts: set.hosts.clone(),
            };
            next.hosts.insert(key, (next.seq, text));
            *set = Arc::new(next);
        });
    }

    /// A host's agent is gone: its rows are no longer sent to a page that asks.
    pub fn forget(&self, host: &str) {
        let key = host_key(host);
        self.0.send_if_modified(|set| {
            if !set.hosts.contains_key(&key) {
                return false;
            }
            let mut hosts = set.hosts.clone();
            hosts.remove(&key);
            *set = Arc::new(StatusSet {
                seq: set.seq + 1,
                hosts,
            });
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
        all.publish("host-a", status(1, 2));
        all.publish("host-b", status(2, 1));
        all.publish("host-a", status(3, 2));
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
        all.publish("host-b", status(4, 1));
        let next = rx.borrow_and_update().clone();
        let again: Vec<&Arc<str>> = next.newer(set.seq).collect();
        assert_eq!(again.len(), 1);
        assert!(again[0].contains("\"t\":4"), "{}", again[0]);
        // a host that left is no longer sent to a page that asks later
        all.forget("host-a");
        assert_eq!(rx.borrow_and_update().newer(0).count(), 1);
        all.forget("host-a");
    }

    #[test]
    fn a_report_of_more_servers_than_a_host_can_run_is_dropped() {
        let all = MinecraftStatuses::default();
        all.publish("host-a", status(1, MAX_SERVERS));
        assert_eq!(all.subscribe().borrow().hosts.len(), 1);
        all.publish("host-b", status(1, MAX_SERVERS + 2));
        assert_eq!(all.subscribe().borrow().hosts.len(), 1);
    }
}
