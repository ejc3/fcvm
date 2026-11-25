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
//! a period (the latest one that came sooner is kept and taken when the gap ends, so the last
//! word of a host that then goes silent is not lost), and only when its rows are whole rows for
//! no more servers than the host said it runs.

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
/// A connection's reports are taken no closer together than this, half the period an agent
/// reports at: of those that come sooner the latest is kept and taken when the gap ends.
pub const MIN_GAP: Duration = Duration::from_millis(STATUS_EVERY_MS / 2);

/// What became of a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Taken {
    /// Sent on to the pages that asked.
    Now,
    /// Inside the gap: kept, to be taken at `until` ([`MinecraftStatuses::flush_at`]) unless a
    /// later one replaces it. `first`: nothing was being kept for the host before it.
    Held { until: Instant, first: bool },
    /// Not from the host's current connection, or its rows are not whole rows for the host's
    /// servers.
    Dropped,
}
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
    /// The latest report that came inside the gap, with when it came.
    held: Option<(MinecraftStatus, Instant, u64)>,
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

    /// The set with `status` (which came at `came`, `received_at` on the coordinator's clock)
    /// as `key`'s rows, taken at `now`.
    fn with(
        &self,
        key: &str,
        status: &MinecraftStatus,
        came: Instant,
        received_at: u64,
        now: Instant,
    ) -> StatusSet {
        let mut next = self.next();
        let text: Arc<str> = serde_json::to_string(&ServerMsg::MinecraftStatus {
            host: key.to_string(),
            received_at,
            status: status.clone(),
        })
        .expect("server messages serialize")
        .into();
        // rows that expired say nothing: they go when anything is taken
        next.hosts
            .retain(|_, r| now.saturating_duration_since(r.received) <= EXPIRE);
        next.hosts.insert(
            key.to_string(),
            Rows {
                seq: next.seq,
                text,
                received: came,
            },
        );
        if let Some(o) = next.owners.get_mut(key) {
            o.last = Some(now);
            o.held = None;
        }
        next
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
                    held: None,
                },
            );
            *set = Arc::new(next);
        });
    }

    /// A host's agent sent its rows over connection `conn`. A report kept for the end of the
    /// gap is taken then by a task of its own.
    pub fn publish(&self, host: &str, conn: u64, status: MinecraftStatus) {
        let taken = self.publish_at(host, conn, status, Instant::now(), unix_ms());
        if let Taken::Held { until, first: true } = taken {
            let (this, host) = (self.clone(), host.to_string());
            tokio::spawn(async move {
                tokio::time::sleep_until(until.into()).await;
                this.flush_at(&host, conn, Instant::now());
            });
        }
    }

    /// [`publish`](Self::publish) at `now` (`received_at` on the coordinator's clock). Dropped
    /// unless `conn` is the host's current connection and `rows` is base64 of whole rows for no
    /// more servers than the host runs. Taken at once when the host's last report is at least
    /// [`MIN_GAP`] old, else kept (in place of any kept before it) for
    /// [`flush_at`](Self::flush_at).
    pub fn publish_at(
        &self,
        host: &str,
        conn: u64,
        status: MinecraftStatus,
        now: Instant,
        received_at: u64,
    ) -> Taken {
        let key = host_key(host);
        let rows = unbase64(&status.rows);
        let mut taken = Taken::Dropped;
        self.0.send_if_modified(|set| {
            let Some(owner) = set.owners.get(&key).filter(|o| o.conn == conn) else {
                return false;
            };
            let most = (owner.servers as usize).min(MAX_SERVERS);
            let whole = rows.as_ref().is_some_and(|r| {
                r.len().is_multiple_of(STATUS_ROW_BYTES) && r.len() / STATUS_ROW_BYTES <= most
            });
            if !whole {
                tracing::warn!(target: "fcvm::fleet::minecraft", %host, chars = status.rows.len(), servers = owner.servers, "dropping a status report whose rows are not whole rows for the host's servers");
                return false;
            }
            if let Some(last) = owner
                .last
                .filter(|last| now.saturating_duration_since(*last) < MIN_GAP)
            {
                // Kept, and no page is told: the set changes only in what it will take later.
                let first = owner.held.is_none();
                let mut same = StatusSet {
                    seq: set.seq,
                    hosts: set.hosts.clone(),
                    owners: set.owners.clone(),
                };
                if let Some(o) = same.owners.get_mut(&key) {
                    o.held = Some((status.clone(), now, received_at));
                }
                *set = Arc::new(same);
                taken = Taken::Held {
                    until: last + MIN_GAP,
                    first,
                };
                return false;
            }
            *set = Arc::new(set.with(&key, &status, now, received_at, now));
            taken = Taken::Now;
            true
        });
        taken
    }

    /// The gap of `host`'s connection `conn` has ended at `now`: the report kept for it, if
    /// one still is, is taken. Whether one was.
    pub fn flush_at(&self, host: &str, conn: u64, now: Instant) -> bool {
        let key = host_key(host);
        self.0.send_if_modified(|set| {
            let Some(owner) = set.owners.get(&key).filter(|o| o.conn == conn) else {
                return false;
            };
            let Some((status, came, received_at)) = owner.held.clone() else {
                return false;
            };
            if owner
                .last
                .is_some_and(|last| now.saturating_duration_since(last) < MIN_GAP)
            {
                return false;
            }
            *set = Arc::new(set.with(&key, &status, came, received_at, now));
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
        assert_eq!(
            Taken::Now,
            all.publish_at("host-a", 1, status(1, 2), t0, 7_000)
        );
        assert_eq!(
            Taken::Now,
            all.publish_at("host-b", 2, status(2, 1), t0, 7_000)
        );
        assert_eq!(
            Taken::Now,
            all.publish_at("host-a", 1, status(3, 2), t0 + S, 8_000)
        );
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
        assert_eq!(
            Taken::Now,
            all.publish_at("host-b", 2, status(4, 1), t0 + S, 8_000)
        );
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
        assert_eq!(Taken::Now, all.publish_at("host-a", 1, status(1, 2), t0, 0));
        assert_eq!(Taken::Now, all.publish_at("host-b", 2, status(2, 2), t0, 0));
        // a few periods missed is not yet silence
        assert_eq!(ts(&all, t0 + 4 * S).len(), 2);
        assert_eq!(ts(&all, t0 + EXPIRE).len(), 2);
        // past that, a page that asks is sent nothing of either host
        assert!(ts(&all, t0 + EXPIRE + Duration::from_millis(1)).is_empty());
        // host-b goes on reporting: only it is sent, and host-a's old rows are gone
        assert_eq!(
            Taken::Now,
            all.publish_at("host-b", 2, status(3, 2), t0 + 6 * S, 0)
        );
        assert_eq!(ts(&all, t0 + 6 * S), [3]);
        assert_eq!(all.subscribe().borrow().len(), 1);
        // and host-a is sent again as soon as it reports again
        assert_eq!(
            Taken::Now,
            all.publish_at("host-a", 1, status(4, 2), t0 + 7 * S, 0)
        );
        let mut both = ts(&all, t0 + 7 * S);
        both.sort();
        assert_eq!(both, [3, 4]);
    }

    #[test]
    fn a_replaced_connection_neither_publishes_nor_takes_away_its_hosts_rows() {
        let all = MinecraftStatuses::default();
        let t0 = Instant::now();
        // a connection whose hello was not accepted publishes nothing
        assert_eq!(
            Taken::Dropped,
            all.publish_at("host-a", 7, status(1, 1), t0, 0)
        );
        assert!(ts(&all, t0).is_empty());
        all.claim("host-a", 7, 1);
        assert_eq!(Taken::Now, all.publish_at("host-a", 7, status(2, 1), t0, 0));
        assert_eq!(ts(&all, t0), [2]);
        // the host dials again while its old connection is still open: the old rows go at once
        all.claim("host-a", 9, 1);
        assert!(ts(&all, t0).is_empty());
        assert_eq!(Taken::Now, all.publish_at("host-a", 9, status(3, 1), t0, 0));
        // the replaced connection's late report and its close change nothing
        assert_eq!(
            Taken::Dropped,
            all.publish_at("host-a", 7, status(4, 1), t0 + S, 0)
        );
        assert_eq!(ts(&all, t0 + S), [3]);
        all.forget("host-a", 7);
        assert_eq!(ts(&all, t0 + S), [3]);
        // the current connection's close takes the rows away
        all.forget("host-a", 9);
        assert!(ts(&all, t0 + S).is_empty());
    }

    #[test]
    fn a_connection_is_heard_no_more_than_twice_a_period_and_its_last_word_is_kept() {
        let all = MinecraftStatuses::default();
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        all.claim("host-a", 1, 1);
        assert_eq!(Taken::Now, all.publish_at("host-a", 1, status(1, 1), t0, 0));
        // A flood inside the gap reaches no page and does not push the gap out; the latest of
        // it is kept for the gap's end.
        let until = t0 + MIN_GAP;
        assert_eq!(
            Taken::Held { until, first: true },
            all.publish_at("host-a", 1, status(7, 1), t0 + ms(1), 1)
        );
        for (at, t) in [(100, 8), (499, 9)] {
            assert_eq!(
                Taken::Held {
                    until,
                    first: false
                },
                all.publish_at("host-a", 1, status(t, 1), t0 + ms(at), at)
            );
        }
        assert_eq!(ts(&all, t0 + ms(499)), [1]);
        // not before the gap ends
        assert!(!all.flush_at("host-a", 1, t0 + ms(499)));
        // at its end the latest is taken, with when it came, and only once
        let mut rx = all.subscribe();
        assert!(all.flush_at("host-a", 1, until));
        let text = rx
            .borrow_and_update()
            .newer(0, until)
            .next()
            .unwrap()
            .to_string();
        let got: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!((&got["t"], &got["received_at"]), (&9.into(), &499.into()));
        assert!(!all.flush_at("host-a", 1, until + ms(600)));
        // The case it is for: a host's last report, the one that says its board emptied, comes
        // inside the gap and nothing follows it.
        assert_eq!(
            Taken::Now,
            all.publish_at("host-a", 1, status(10, 1), until + S, 0)
        );
        let last = until + S + ms(200);
        assert_eq!(
            Taken::Held {
                until: until + S + MIN_GAP,
                first: true
            },
            all.publish_at("host-a", 1, status(11, 0), last, 0)
        );
        assert!(all.flush_at("host-a", 1, until + S + MIN_GAP));
        assert_eq!(ts(&all, until + S + MIN_GAP), [11]);
        // a report that comes after the gap is taken at once and drops what was kept
        assert_eq!(
            Taken::Now,
            all.publish_at("host-a", 1, status(12, 1), until + 3 * S, 0)
        );
        assert!(matches!(
            all.publish_at("host-a", 1, status(13, 1), until + 3 * S + ms(10), 0),
            Taken::Held { first: true, .. }
        ));
        assert_eq!(
            Taken::Now,
            all.publish_at("host-a", 1, status(14, 1), until + 4 * S, 0)
        );
        assert!(!all.flush_at("host-a", 1, until + 5 * S));
        assert_eq!(ts(&all, until + 5 * S), [14]);
        // what was kept for a connection the host replaced is never taken
        assert!(matches!(
            all.publish_at("host-a", 1, status(15, 1), until + 4 * S + ms(10), 0),
            Taken::Held { .. }
        ));
        all.claim("host-a", 2, 1);
        assert!(!all.flush_at("host-a", 1, until + 5 * S));
        assert!(!all.flush_at("host-a", 2, until + 5 * S));
        assert!(ts(&all, until + 5 * S).is_empty());
        // rows that are not whole are dropped inside the gap too, and replace nothing kept
        assert_eq!(
            Taken::Now,
            all.publish_at("host-a", 2, status(16, 1), until + 6 * S, 0)
        );
        assert!(matches!(
            all.publish_at("host-a", 2, status(17, 1), until + 6 * S + ms(10), 0),
            Taken::Held { .. }
        ));
        assert_eq!(
            Taken::Dropped,
            all.publish_at("host-a", 2, status(18, 2), until + 6 * S + ms(20), 0)
        );
        assert!(all.flush_at("host-a", 2, until + 7 * S));
        assert_eq!(ts(&all, until + 7 * S), [17]);
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
        assert_eq!(
            Taken::Dropped,
            all.publish_at("host-a", 1, with("!!!!".into()), t0, 0)
        );
        assert_eq!(
            Taken::Dropped,
            all.publish_at(
                "host-a",
                1,
                with(fleet_core::minecraft::base64(&[3u8; 12])),
                t0,
                0
            )
        );
        assert_eq!(
            Taken::Dropped,
            all.publish_at("host-a", 1, status(5, 3), t0, 0)
        );
        assert!(ts(&all, t0).is_empty());
        // a dropped report does not start the gap: the next good one is taken at once
        assert_eq!(Taken::Now, all.publish_at("host-a", 1, status(6, 2), t0, 0));
        // fewer rows than servers (a host that runs fewer just now), and none at all, are whole
        assert_eq!(
            Taken::Now,
            all.publish_at("host-a", 1, status(7, 1), t0 + S, 0)
        );
        assert_eq!(
            Taken::Now,
            all.publish_at("host-a", 1, status(8, 0), t0 + 2 * S, 0)
        );
        assert_eq!(ts(&all, t0 + 2 * S), [8]);
        // no hello can raise the bound past MAX_SERVERS
        all.claim("host-b", 2, u32::MAX);
        assert_eq!(
            Taken::Now,
            all.publish_at("host-b", 2, status(1, MAX_SERVERS), t0, 0)
        );
        assert_eq!(
            Taken::Dropped,
            all.publish_at("host-b", 2, status(2, MAX_SERVERS + 1), t0 + S, 0)
        );
    }
}
