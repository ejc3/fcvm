//! `apply(state, event) -> state`: the only way the fleet state changes. No clock, no I/O,
//! no randomness; the time is in the event (and a claim's token arrives as the intent's
//! `secret`).

use crate::event::{Event, Intent};
use crate::history;
use crate::protocol::{AgentMsg, HealthReport, SlotReport};
use crate::scene::{drag, Keyframe, Light, Word, MAX_LIGHTS, MAX_WORDS};
use crate::state::*;
use crate::tiling::{bands, grid_in, sides, Half, Rect};
use std::collections::BTreeMap;

/// How long a reclaimed host stays in the failure list after it is gone.
pub const RECLAIMED_KEEP_MS: u64 = 60_000;
/// Races counted in the scoreboard's medians.
pub const SCOREBOARD_RACES: usize = 20;

/// Give `before` the bookkeeping of `after`: state that nobody sees and that a tick moves on
/// its own (the per-client limits it forgets, the budget's round clock, smoothed fit and
/// count of rounds), so a tick that only moves it keeps the version. Each of these changes
/// with a visible change or a receipt when it matters (an intent records its client's time
/// with its receipt; a budget change is in the view).
fn same_bookkeeping(before: &mut Fleet, after: &Fleet) {
    before.last_word_at = after.last_word_at.clone();
    before.last_light_at = after.last_light_at.clone();
    before.last_relaunch_at = after.last_relaunch_at.clone();
    before.last_claim_end = after.last_claim_end.clone();
    before.last_windows_reset_at = after.last_windows_reset_at.clone();
    before.last_windows_reset_all_at = after.last_windows_reset_all_at.clone();
    before.last_windows_claim_end = after.last_windows_claim_end.clone();
    before.last_minecraft_reset_at = after.last_minecraft_reset_at.clone();
    before.last_minecraft_reset_all_at = after.last_minecraft_reset_all_at.clone();
    before.last_minecraft_fork_at = after.last_minecraft_fork_at.clone();
    before.last_minecraft_claim_end = after.last_minecraft_claim_end.clone();
    before.history_ready_at = after.history_ready_at;
    before.budget.next_round_at = after.budget.next_round_at;
    before.budget.smoothed = after.budget.smoothed;
    before.budget.up_rounds = after.budget.up_rounds;
}

/// Apply one event. The version moves exactly when the state changed; an intent id seen
/// before changes nothing (its first receipt stays in `state.receipts`).
pub fn apply(mut state: State, event: &Event) -> State {
    let before = state.fleet.clone();
    let receipt = apply_inner(&mut state, event);
    // Retile, step the power state on what is placed, and retile again when that changed
    // the power (asleep, the EC2 side has no tiles).
    retile(&mut state.fleet, &state.config, event.at());
    let power = state.fleet.power.state.clone();
    power_step(&mut state, event.at());
    if state.fleet.power.state != power {
        retile(&mut state.fleet, &state.config, event.at());
    }
    let mut before = before;
    same_bookkeeping(&mut before, &state.fleet);
    let changed = receipt.is_some() || state.fleet != before;
    if changed {
        state.version += 1;
    }
    if let Some(mut r) = receipt {
        r.version = state.version;
        state.receipt_order.push_back(r.id.clone());
        state.receipts.insert(r.id.clone(), r);
        while state.receipt_order.len() > MAX_RECEIPTS {
            if let Some(old) = state.receipt_order.pop_front() {
                state.receipts.remove(&old);
            }
        }
    }
    state
}

/// The key the per-client limits use: the client address, else the viewer id.
pub fn client_key<'a>(viewer: &'a str, client: Option<&'a str>) -> &'a str {
    client.unwrap_or(viewer)
}

/// The checks `apply` would refuse a `word` on before its text check, so the coordinator
/// spends no text-check call on an intent that is refused anyway. Returns the refusal
/// reason, or `None` when the intent should go on to the text check.
pub fn precheck_word(
    state: &State,
    client: &str,
    args: &serde_json::Value,
    at: u64,
) -> Option<String> {
    match Intent::parse("word", args) {
        Err(reason) => Some(reason),
        Ok(_) => word_rate(state, client, at).err(),
    }
}

/// The viewers' words in the sky now, which a new word must be checked together with. A
/// system word is the wall's own and is not part of what is checked, so one placed while a
/// viewer's word is being checked does not make that check stale.
pub fn sky(state: &State) -> Vec<String> {
    state
        .fleet
        .scene
        .words
        .iter()
        .filter(|w| !w.system)
        .map(|w| w.text.clone())
        .collect()
}

fn word_rate(state: &State, client: &str, at: u64) -> Result<(), String> {
    rate(
        &state.fleet.last_word_at,
        client,
        at,
        state.config.word_interval_ms,
        "word",
    )
}

/// Refuse when this client's last committed intent of this kind is less than
/// `interval_ms` old.
fn rate(
    last: &BTreeMap<String, u64>,
    client: &str,
    at: u64,
    interval_ms: u64,
    what: &str,
) -> Result<(), String> {
    match last.get(client) {
        Some(&t) if at < t + interval_ms => Err(format!(
            "one {what} per {} s per client",
            interval_ms.div_ceil(1000)
        )),
        _ => Ok(()),
    }
}

fn apply_inner(state: &mut State, event: &Event) -> Option<Receipt> {
    let at = event.at();
    match event {
        Event::Intent {
            viewer,
            client,
            id,
            name,
            args,
            check,
            secret,
            ..
        } => {
            if state.receipts.contains_key(id) {
                return None;
            }
            let ctx = Ctx {
                viewer,
                client: client_key(viewer, client.as_deref()),
                id,
                secret: secret.as_deref(),
                at,
            };
            let result = Intent::parse(name, args)
                .and_then(|intent| intent_effect(state, &ctx, &intent, check.as_ref()));
            let (outcome, reason, summary) = match result {
                Ok(summary) => (Outcome::Committed, None, summary),
                // The reason reaches the public actions feed and the viewer's receipt.
                Err(reason) => {
                    let reason = crate::view::hide_account_ids(&reason);
                    (Outcome::Refused, Some(reason.clone()), reason)
                }
            };
            // A refused word's text is never shown: its args are left out and the summary
            // is the reason only.
            let shown_args = if name == "word" && outcome == Outcome::Refused {
                serde_json::json!({})
            } else {
                args.clone()
            };
            push_action(
                &mut state.fleet,
                Action {
                    at,
                    viewer: viewer.clone(),
                    id: id.clone(),
                    name: name.clone(),
                    args: shown_args,
                    summary,
                    outcome,
                },
            );
            Some(Receipt {
                id: id.clone(),
                outcome,
                reason,
                version: 0,
                check: check.clone().map(|mut k| {
                    k.reason = crate::view::hide_account_ids(&k.reason);
                    k
                }),
            })
        }
        Event::ViewerJoined {
            viewer,
            client,
            returning,
            ..
        } => {
            let key = client_key(viewer, client.as_deref()).to_string();
            state.fleet.viewers.insert(viewer.clone(), key);
            if !returning {
                state.fleet.power.last_arrival_at = Some(at);
            }
            None
        }
        Event::Visibility {
            viewer, visible, ..
        } => {
            let f = &mut state.fleet;
            if f.viewers.contains_key(viewer) {
                if *visible {
                    f.hidden.remove(viewer);
                } else {
                    f.hidden.insert(viewer.clone());
                }
            }
            None
        }
        Event::Published { slot, names, .. } => {
            let c = &state.config;
            if slot == crate::state::EC2_AGGREGATE_ID {
                if names.names(
                    c.ec2_aggregate_ipv6.as_deref(),
                    c.ec2_aggregate_ipv4.as_deref(),
                    None,
                ) {
                    state.fleet.ec2_aggregate_published = Some(names.clone());
                }
                return None;
            }
            let f = &mut state.fleet;
            // A host aggregator's names, for the addresses it reports now.
            if let Some(h) = f
                .hosts
                .values_mut()
                .find(|h| crate::state::aggregate_id(&h.name).as_deref() == Some(slot.as_str()))
            {
                if names.names(
                    h.aggregate_ipv6.as_deref(),
                    h.aggregate_ipv4.as_deref(),
                    h.aggregate_v4_port,
                ) {
                    h.aggregate_published = Some(names.clone());
                }
                return None;
            }
            if let Some(s) = f
                .slots
                .values_mut()
                .chain(f.ec2.values_mut())
                .chain(f.phones.values_mut())
                .chain(f.windows.values_mut())
                .chain(f.minecraft.values_mut())
                .find(|s| s.id == *slot)
            {
                // Only for the launch the slot is on: a slower write for an earlier launch
                // must not replace a newer one's names.
                if s.observed.generation == names.generation
                    && s.published
                        .as_ref()
                        .is_none_or(|p| p.generation <= names.generation)
                {
                    s.published = Some(names.clone());
                }
            }
            None
        }
        Event::HostsExpected { hosts, .. } => {
            state.fleet.expected_hosts = hosts.as_ref().map(|h| h.iter().cloned().collect());
            None
        }
        Event::ScaleInstances { instances, .. } => {
            let p = &mut state.fleet.power;
            if wake_open(p) {
                for i in instances {
                    let w = p.hosts.entry(i.id.clone()).or_default();
                    w.pending_at.get_or_insert(at);
                    if i.lifecycle == "InService" {
                        w.booted_at.get_or_insert(at);
                    }
                }
            }
            None
        }
        Event::ViewerLeft { viewer, .. } => {
            state.fleet.viewers.remove(viewer);
            state.fleet.hidden.remove(viewer);
            // A viewer that leaves gives its GPU back.
            let mine: Vec<u32> = state
                .fleet
                .slots
                .iter()
                .filter(|(_, s)| s.claim.as_ref().is_some_and(|c| c.viewer == *viewer))
                .map(|(n, _)| *n)
                .collect();
            for n in mine {
                release(&mut state.fleet, n, at);
            }
            let phones: Vec<u32> = state
                .fleet
                .phones
                .iter()
                .filter(|(_, s)| phone_claim(s).is_some_and(|c| c.viewer == *viewer))
                .map(|(n, _)| *n)
                .collect();
            for n in phones {
                release_phone(&mut state.fleet, n, at);
            }
            let sandboxes: Vec<u32> = state
                .fleet
                .windows
                .iter()
                .filter(|(_, s)| windows_claim(s).is_some_and(|c| c.viewer == *viewer))
                .map(|(n, _)| *n)
                .collect();
            for n in sandboxes {
                release_windows(&mut state.fleet, n, at);
            }
            let servers: Vec<u32> = state
                .fleet
                .minecraft
                .iter()
                .filter(|(_, s)| minecraft_claim(s).is_some_and(|c| c.viewer == *viewer))
                .map(|(n, _)| *n)
                .collect();
            for n in servers {
                release_minecraft(&mut state.fleet, n, at);
            }
            None
        }
        Event::Tick { .. } => {
            tick(state, at);
            budget_round(&mut state.fleet, at);
            let awake = state.fleet.power.state != "asleep";
            let cfg = state.config.potato.clone();
            state.fleet.potato.tick(at, awake, &cfg);
            None
        }
        Event::Ec2Health { report, .. } => {
            health(&mut state.fleet, None, report);
            None
        }
        Event::Agent { host, msg, .. } => {
            match msg {
                AgentMsg::Hello {
                    host: named,
                    arch,
                    gpus,
                    capacity,
                    store_ready_at,
                    boot,
                    phones,
                    phone_actions,
                    potato_players,
                    windows,
                    windows_actions,
                    minecraft,
                    minecraft_server,
                } => {
                    // The connection names the host; a hello naming another is ignored.
                    if named == host {
                        let cfg = state.config.potato.clone();
                        state
                            .fleet
                            .potato
                            .agent_hello(host, *potato_players, at, &cfg);
                        // A hello again on the same connection keeps the image it reported.
                        let image = state
                            .fleet
                            .hosts
                            .get(host)
                            .and_then(|h| h.windows_image.clone());
                        hello(&mut state.fleet, host, arch, gpus, *capacity, at);
                        if let Some(h) = state.fleet.hosts.get_mut(host) {
                            h.phone_actions = phone_actions.clone();
                            h.windows_actions = windows_actions.clone();
                            h.windows_image = image.filter(|_| *windows > 0);
                            h.minecraft_server =
                                minecraft_server.clone().filter(|_| *minecraft > 0);
                        }
                        phone_hello(&mut state.fleet, host, gpus, phones, at);
                        windows_hello(&mut state.fleet, host, *windows, at);
                        minecraft_hello(&mut state.fleet, host, *minecraft, at);
                        let p = &mut state.fleet.power;
                        if wake_open(p) {
                            let woke = p.woke_at;
                            let w = p.hosts.entry(host.clone()).or_default();
                            // The host's own clock: kept only when it falls inside this wake.
                            let inside = |t: &u64| woke.is_some_and(|w| *t >= w) && *t <= at;
                            if w.agent_at.is_none() {
                                w.agent_at = Some(at);
                                if let Some(t) = store_ready_at.filter(inside) {
                                    w.store_ready_at.get_or_insert(t);
                                }
                                w.boot = boot
                                    .iter()
                                    .filter(|(_, t)| inside(t))
                                    .map(|(k, t)| (k.clone(), *t))
                                    .collect();
                            }
                        }
                    }
                }
                AgentMsg::Slot(r) => {
                    if let Some(n) = slot_number(&r.id) {
                        observe_slot(&mut state.fleet, Half::Fcvm, n, Some(host), r, at);
                    } else if let Some(n) = phone_number(&r.id) {
                        observe_phone(&mut state.fleet, n, host, r, at);
                    } else if let Some(n) = windows_number(&r.id) {
                        observe_windows(&mut state.fleet, n, host, r, at);
                    } else if let Some(n) = minecraft_number(&r.id) {
                        observe_minecraft(&mut state.fleet, n, host, r, at);
                    }
                }
                AgentMsg::Health(h) => health(&mut state.fleet, Some(host), h),
                AgentMsg::Draining { .. } => {
                    if let Some(h) = state.fleet.hosts.get_mut(host) {
                        h.draining = true;
                    }
                    state
                        .fleet
                        .reclaimed
                        .entry(host.clone())
                        .or_insert((at, None));
                    drop_from_races(
                        &mut state.fleet,
                        |s| s.host.as_deref() == Some(host.as_str()),
                        at,
                    );
                }
                AgentMsg::Aggregate {
                    frame_url,
                    ipv6,
                    ipv4,
                    v4_port,
                } => {
                    if let Some(h) = state.fleet.hosts.get_mut(host) {
                        h.aggregate_url = Some(frame_url.clone());
                        h.aggregate_ipv6 = ipv6.clone();
                        h.aggregate_ipv4 = ipv4.clone();
                        h.aggregate_v4_port = *v4_port;
                    }
                }
                AgentMsg::Scrub { ok, .. } => {
                    let c = &mut state.fleet.counters;
                    if *ok {
                        c.scrubs += 1;
                    } else {
                        c.scrub_failures += 1;
                    }
                }
                AgentMsg::Quarantine {
                    sbdf,
                    error,
                    report,
                    ..
                } => {
                    let key = gpu_key(host, sbdf);
                    let released = report.is_some()
                        && state.fleet.released_reports.get(&key) == report.as_ref();
                    if !released {
                        quarantine(&mut state.fleet, host, sbdf, error, report.clone(), at);
                    }
                }
                AgentMsg::Potato(e) => {
                    let cfg = state.config.potato.clone();
                    state.fleet.potato.event(host, e, at, &cfg);
                }
                AgentMsg::WindowsImage(image) => {
                    if let Some(h) = state.fleet.hosts.get_mut(host) {
                        h.windows_image = Some(image.clone());
                    }
                }
            }
            None
        }
        Event::AgentGone { host, .. } => {
            let cfg = state.config.potato.clone();
            state.fleet.potato.agent_gone(host, at, &cfg);
            let f = &mut state.fleet;
            f.hosts.remove(host);
            f.slots.retain(|_, s| s.host.as_deref() != Some(host));
            f.phones.retain(|_, s| s.host.as_deref() != Some(host));
            forget_windows(f, |s| s.host.as_deref() == Some(host.as_str()), at);
            forget_minecraft(f, |s| s.host.as_deref() == Some(host.as_str()), at);
            // A host the coordinator scaled away is not a reclaimed one.
            if f.power.state != "asleep" || f.reclaimed.contains_key(host) {
                f.reclaimed.entry(host.clone()).or_insert((at, None)).1 = Some(at);
            }
            drop_missing_from_races(f, at);
            None
        }
        Event::Ec2 { report, .. } => {
            if let Some(n) = ec2_number(&report.id) {
                observe_slot(&mut state.fleet, Half::Ec2, n, None, report, at);
            }
            None
        }
        Event::Degraded {
            component, reason, ..
        } => {
            let f = &mut state.fleet;
            match reason {
                Some(r) => {
                    let since = f.degraded.get(component).map_or(at, |(_, s)| *s);
                    f.degraded.insert(component.clone(), (r.clone(), since));
                }
                None => {
                    f.degraded.remove(component);
                }
            }
            None
        }
        Event::ReleaseQuarantine { host, sbdf, .. } => {
            let key = gpu_key(host, sbdf);
            if let Some(report) = state.fleet.quarantined.remove(&key).and_then(|q| q.report) {
                state.fleet.released_reports.insert(key, report);
            }
            None
        }
    }
}

/// A tile on the wall with a renderer behind it: a placed fcvm slot, or a placed EC2 tile
/// with an instance. An EC2 tile without one holds its place, dark, and is no GPU of the
/// wall's: not counted, not waited on.
pub fn renders_on_wall(s: &Slot) -> bool {
    s.position.is_some() && (s.half == Half::Fcvm || s.observed.vm.is_some())
}

/// A race whose halves are not all done.
pub fn race_running(f: &Fleet) -> bool {
    f.races.back().is_some_and(|r| {
        r.fcvm.done_at.is_none() && !r.fcvm.tiles.is_empty()
            || r.ec2.done_at.is_none() && !r.ec2.tiles.is_empty()
    })
}

/// Whether the image shows what it will keep showing: no race is running and no placed
/// tile (claimed GPUs aside, they are black on the wall) is starting or relaunching. A tile
/// that failed or waits for capacity is dark for a while and does not hold this back.
pub fn image_settled(f: &Fleet) -> bool {
    let starting = |s: &Slot| {
        renders_on_wall(s)
            && s.claim.is_none()
            && (s.observed.generation < s.generation
                || matches!(
                    s.observed.phase.as_str(),
                    "pending" | "launching" | "stopping" | "restoring" | "starting"
                ))
    };
    !race_running(f) && !f.slots.values().chain(f.ec2.values()).any(starting)
}

/// Every placed fcvm tile runs its current generation, and every EC2 tile does too or is
/// waiting for capacity (a wait the EC2 side shows on its own).
fn image_complete(f: &Fleet) -> bool {
    let running = |s: &Slot| s.observed.phase == "running" && s.observed.generation >= s.generation;
    f.slots.values().chain(f.ec2.values()).any(renders_on_wall)
        && f.slots.values().all(|s| s.position.is_none() || running(s))
        && f.ec2
            .values()
            .all(|s| running(s) || s.observed.phase == "waiting_capacity")
}

/// Sleep after `idle_scale_down_secs` with no active viewer, or once no viewer has arrived
/// for `sleep_without_new_viewer_secs` (never during a claim or a race); wake on an active
/// viewer while a viewer has arrived within that limit, and be awake once the image is
/// complete.
fn power_step(state: &mut State, at: u64) {
    if !state.config.power_managed {
        return;
    }
    let idle_ms = state.config.idle_scale_down_secs * 1000;
    let interval = state.config.race_interval_secs;
    let no_arrival = crate::view::no_arrival_deadline(&state.fleet.power, &state.config)
        .is_some_and(|t| at >= t);
    let f = &mut state.fleet;
    let active = f.viewers.keys().filter(|v| !f.hidden.contains(*v)).count();
    if active > 0 {
        f.power.idle_since = None;
    } else {
        f.power.idle_since.get_or_insert(at);
    }
    let claims = f.slots.values().any(|s| s.claim.is_some())
        || f.phones.values().any(|s| phone_claim(s).is_some())
        || f.windows.values().any(|s| windows_claim(s).is_some())
        || f.minecraft.values().any(|s| minecraft_claim(s).is_some());
    let racing = race_running(f);
    if f.power.state == "asleep" {
        // Connected pages do not wake it once the limit has run out: an arrival does.
        if active > 0 && !no_arrival {
            f.power.state = "waking".into();
            f.power.woke_at = Some(at);
            f.power.asleep_since = None;
            f.power.hosts.clear();
        }
        return;
    }
    let idle = active == 0 && f.power.idle_since.is_some_and(|t| at >= t + idle_ms);
    if (idle || no_arrival) && !claims && !racing {
        f.power.state = "asleep".into();
        f.power.asleep_since = Some(at);
        f.power.woke_at = None;
        f.power.hosts.clear();
        f.next_race_at = None;
        return;
    }
    if wake_open(&f.power) {
        let hosts = f.hosts.clone();
        for (id, w) in f.power.hosts.iter_mut() {
            if w.rendering_at.is_some() || !hosts.contains_key(id) {
                continue;
            }
            let mine: Vec<&Slot> = f
                .slots
                .values()
                .filter(|s| s.host.as_deref() == Some(id))
                .collect();
            if mine.iter().any(|s| s.position.is_some())
                && mine.iter().all(|s| {
                    s.position.is_none()
                        || (s.observed.phase == "running" && s.observed.generation >= s.generation)
                })
            {
                w.rendering_at = Some(at);
            }
        }
    }
    if f.power.state == "waking" && image_complete(f) {
        let started = f.power.woke_at.unwrap_or(at);
        let gpus = f
            .slots
            .values()
            .chain(f.ec2.values())
            .filter(|s| renders_on_wall(s))
            .count();
        f.power.wakes.push_back(Wake {
            started_at: started,
            awake_at: at,
            gpus,
            ms: at - started,
        });
        while f.power.wakes.len() > MAX_WAKES {
            f.power.wakes.pop_front();
        }
        f.power.state = "awake".into();
        f.next_race_at = interval.map(|s| at + s * 1000);
    }
}

/// Whether the hosts' progress still belongs to the current wake: while it wakes, and after
/// it is awake until every host recorded for it renders. The fleet is awake once the image
/// is complete, which the EC2 half can make before any GPU host's agent has said hello
/// (staging, 2026-10-25: awake 53 s into a wake whose host rendered at 150 s), so the
/// host's later phases are recorded too. A host that never renders keeps the record open
/// until the fleet sleeps.
pub fn wake_open(p: &crate::state::Power) -> bool {
    p.woke_at.is_some()
        && (p.state == "waking"
            || p.hosts.is_empty()
            || p.hosts.values().any(|w| w.rendering_at.is_none()))
}

/// Whether the GPU behind a slot is quarantined.
pub fn is_quarantined(f: &Fleet, s: &Slot) -> bool {
    match (&s.host, &s.gpu) {
        (Some(h), Some(g)) => f.quarantined.contains_key(&gpu_key(h, &g.sbdf)),
        _ => false,
    }
}

/// Take a GPU out of rotation: its slot leaves the tiling (and any race and claim), and
/// stays out until an operator releases it.
fn quarantine(f: &mut Fleet, host: &str, sbdf: &str, error: &str, report: Option<String>, at: u64) {
    let key = gpu_key(host, sbdf);
    if f.quarantined.contains_key(&key) {
        return;
    }
    f.quarantined.insert(
        key,
        Quarantined {
            host: host.to_string(),
            sbdf: sbdf.to_string(),
            error: error.to_string(),
            since: at,
            report,
        },
    );
    let on_gpu = |s: &Slot| {
        s.host.as_deref() == Some(host) && s.gpu.as_ref().is_some_and(|g| g.sbdf == sbdf)
    };
    let claimed: Vec<u32> = f
        .slots
        .iter()
        .filter(|(_, s)| on_gpu(s) && s.claim.is_some())
        .map(|(n, _)| *n)
        .collect();
    for n in claimed {
        release(f, n, at);
    }
    drop_from_races(f, on_gpu, at);
}

/// Count a restore in the totals and in this minute's bucket.
fn count_restore(c: &mut Counters, at: u64) {
    c.restores += 1;
    let minute = at / 60_000;
    match c.restores_by_minute.back_mut() {
        Some((m, n)) if *m == minute => *n += 1,
        _ => c.restores_by_minute.push_back((minute, 1)),
    }
}

struct Ctx<'a> {
    viewer: &'a str,
    client: &'a str,
    id: &'a str,
    secret: Option<&'a str>,
    at: u64,
}

fn push_action(f: &mut Fleet, a: Action) {
    f.actions.push_back(a);
    while f.actions.len() > MAX_ACTIONS {
        f.actions.pop_front();
    }
}

fn intent_effect(
    state: &mut State,
    c: &Ctx,
    intent: &Intent,
    check: Option<&TextCheck>,
) -> Result<String, String> {
    let at = c.at;
    let by = format!("viewer:{}", c.viewer);
    match intent {
        Intent::Relaunch { tile } => {
            let (half, n) = state
                .find_slot(tile)
                .ok_or_else(|| format!("no GPU behind {tile}"))?;
            rate(
                &state.fleet.last_relaunch_at,
                c.client,
                at,
                state.config.relaunch_interval_ms,
                "relaunch",
            )?;
            let f = &mut state.fleet;
            let slot = match half {
                Half::Fcvm => f.slots.get_mut(&n),
                Half::Ec2 => f.ec2.get_mut(&n),
            }
            .expect("find_slot returned a live slot");
            if half == Half::Fcvm && host_draining(&f.hosts, slot) {
                return Err(format!("{} is on a draining host", slot.id));
            }
            if slot.claim.is_some() {
                return Err(format!("{} is a viewer's own GPU right now", slot.id));
            }
            bump(slot, &by, at);
            let summary = format!("relaunch {}", slot.id);
            f.last_relaunch_at.insert(c.client.to_string(), at);
            Ok(summary)
        }
        Intent::Wake => {
            if !state.config.power_managed {
                return Err("this fleet does not sleep".into());
            }
            if !state.fleet.viewers.contains_key(c.viewer) {
                return Err("only a connected page can wake the fleet".into());
            }
            state.fleet.power.last_arrival_at = Some(at);
            Ok("wake".into())
        }
        Intent::RestartNow => {
            if state.fleet.power.state == "asleep" {
                return Err("the GPUs are asleep".into());
            }
            if let Some(until) = state.fleet.cooldown_until.filter(|u| *u > at) {
                return Err(format!(
                    "restart is cooling down for another {} s",
                    (until - at).div_ceil(1000)
                ));
            }
            let id = start_race(state, &by, at);
            Ok(format!("race {id}"))
        }
        Intent::Nudge { target } => {
            state.fleet.director.vote(c.client, target, c.id, c.at)?;
            Ok(format!("nudge {target}"))
        }
        Intent::Word { text, text_style } => {
            word_rate(state, c.client, at)?;
            match check {
                None => return Err("the text check did not run".into()),
                // The coordinator words an error's reason for the viewer ("words are
                // turned off right now"); the detail is in its log.
                Some(k) if k.verdict == "error" => return Err(k.reason.clone()),
                Some(k) if !k.allowed() => {
                    return Err(format!("the text check refused it: {}", k.reason))
                }
                Some(k) if k.sky != sky(state) => {
                    return Err(
                        "the words in the sky changed while this one was checked; send it again"
                            .into(),
                    )
                }
                Some(_) => {}
            }
            let f = &mut state.fleet;
            f.last_word_at.insert(c.client.to_string(), at);
            let t = f.scene.t_at(at);
            f.scene.words.push(Word {
                text: text.clone(),
                t,
                text_style: *text_style,
                system: false,
            });
            trim_viewer_words(&mut f.scene.words);
            // The newest word is the one drawn, so a viewer's word replaces a system word
            // at once; system words wait until it has left the sky and the gap has passed.
            if state.config.history_idle_secs > 0 {
                f.history_ready_at = Some(
                    at + history::hold_ms(*text_style) + state.config.history_idle_secs * 1000,
                );
            }
            Ok(format!("word {text:?}"))
        }
        Intent::Light { pos, color } => {
            let cap = state.config.lights_per_client;
            let f = &mut state.fleet;
            rate(
                &f.last_light_at,
                c.client,
                at,
                state.config.light_interval_ms,
                "light",
            )?;
            f.last_light_at.insert(c.client.to_string(), at);
            // This client's oldest lights make room.
            while f.light_owners.iter().filter(|o| *o == c.client).count() >= cap.max(1) {
                let i = f.light_owners.iter().position(|o| o == c.client).unwrap();
                f.light_owners.remove(i);
                f.scene.lights.remove(i);
            }
            let t = f.scene.t_at(at);
            f.scene.lights.push(Light {
                pos: *pos,
                color: *color,
                t,
            });
            f.light_owners.push(c.client.to_string());
            while f.scene.lights.len() > MAX_LIGHTS {
                f.scene.lights.remove(0);
                f.light_owners.remove(0);
            }
            Ok("light".into())
        }
        // Asleep with pages still connected (no new viewer for the limit), a claim would hold
        // a GPU the fleet is not running and keep nothing awake: refuse it, as restart_now.
        Intent::ClaimGpu
        | Intent::ClaimPhone { .. }
        | Intent::ClaimWindows { .. }
        | Intent::ClaimMinecraft { .. }
            if state.fleet.power.state == "asleep" =>
        {
            Err("the GPUs are asleep".into())
        }
        Intent::WindowsReset { slot } => {
            expire_windows(&mut state.fleet, at);
            windows_reset(state, c, slot.as_deref())
        }
        Intent::ClaimWindows { slot } => {
            expire_windows(&mut state.fleet, at);
            claim_windows(state, c, slot.as_deref())
        }
        Intent::ReleaseWindows { claim } => {
            expire_windows(&mut state.fleet, at);
            let n = own_windows(&state.fleet, c.viewer).ok_or("this viewer holds no sandbox")?;
            check_windows_claim(&state.fleet, n, claim.as_deref())?;
            release_windows(&mut state.fleet, n, at);
            Ok("release sandbox".into())
        }
        Intent::WindowsAction { claim, action } => {
            expire_windows(&mut state.fleet, at);
            windows_action(state, c, claim.as_deref(), *action)
        }
        Intent::MinecraftReset { slot } => {
            expire_minecraft(&mut state.fleet, at);
            minecraft_reset(state, c, slot.as_deref())
        }
        Intent::MinecraftFork { slot } => {
            expire_minecraft(&mut state.fleet, at);
            minecraft_fork(state, c, slot)
        }
        Intent::ClaimMinecraft { slot } => {
            expire_minecraft(&mut state.fleet, at);
            claim_minecraft(state, c, slot.as_deref())
        }
        Intent::ReleaseMinecraft { claim } => {
            expire_minecraft(&mut state.fleet, at);
            let n = own_minecraft(&state.fleet, c.viewer).ok_or("this viewer holds no server")?;
            check_minecraft_claim(&state.fleet, n, claim.as_deref())?;
            release_minecraft(&mut state.fleet, n, at);
            Ok("release server".into())
        }
        Intent::ClaimGpu => claim(state, c),
        Intent::ClaimPhone { phone } => claim_phone(state, c, phone.as_deref()),
        Intent::ReleasePhone { claim } => {
            let n = own_phone(&state.fleet, c.viewer).ok_or("this viewer holds no phone")?;
            check_phone_claim(&state.fleet, n, claim.as_deref())?;
            release_phone(&mut state.fleet, n, at);
            Ok("release phone".into())
        }
        Intent::PhoneAction {
            claim,
            action,
            point,
        } => phone_action(state, c, claim.as_deref(), *action, *point),
        Intent::PhonesZoom { out } => phones_zoom(state, c, *out),
        Intent::PotatoFaster => {
            let cfg = state.config.potato.clone();
            match state.fleet.potato.faster(c.client, at, &cfg) {
                crate::potato::PotatoIntent::Done => Ok("faster".into()),
                crate::potato::PotatoIntent::Refused(r) => Err(r),
            }
        }
        Intent::PotatoRestart => {
            let cfg = state.config.potato.clone();
            match state.fleet.potato.restart(c.client, at, &cfg) {
                crate::potato::PotatoIntent::Done => Ok("restart".into()),
                crate::potato::PotatoIntent::Refused(r) => Err(r),
            }
        }
        Intent::ReleaseGpu { claim } => {
            let n = own_claim(&state.fleet, c.viewer).ok_or("this viewer holds no GPU")?;
            let session = &state.fleet.slots[&n].claim.as_ref().unwrap().session;
            if claim.as_ref().is_some_and(|k| k != session) {
                return Err(format!(
                    "{:?} is not this viewer's claim",
                    claim.as_deref().unwrap()
                ));
            }
            release(&mut state.fleet, n, at);
            Ok("release".into())
        }
        Intent::ClaimCamera {
            yaw_deg,
            pitch_deg,
            dive,
        } => {
            let n = own_claim(&state.fleet, c.viewer)
                .ok_or("claim_camera needs a GPU of your own (claim_gpu)")?;
            let now = state.fleet.scene.t_at(at);
            let claim = state
                .fleet
                .slots
                .get_mut(&n)
                .unwrap()
                .claim
                .as_mut()
                .unwrap();
            // A drag on a flying camera takes it from the autopilot: it eases to rest
            // along its path while it turns by the drag.
            let (from, ease) = if claim.autopilot {
                claim.autopilot = false;
                claim.flight_t0 = None;
                (
                    crate::autopilot::camera_at(claim, now + crate::autopilot::TAKEOVER_SECS / 2.0),
                    crate::autopilot::TAKEOVER_SECS,
                )
            } else {
                (
                    crate::autopilot::camera_at(claim, now),
                    crate::scene::DRAG_EASE_SECS,
                )
            };
            let mut cam = crate::scene::Scene {
                epoch_unix_ms: 0,
                camera: claim.camera.clone(),
                time_of_day: 0.0,
                words: vec![],
                lights: vec![],
                weather: vec![],
            };
            let to = Keyframe {
                intent: Some(c.id.to_string()),
                ..drag(&from, *yaw_deg, *pitch_deg, *dive)
            };
            cam.steer_over(now, to, ease);
            claim.camera = cam.camera;
            Ok("claim camera".into())
        }
        Intent::ClaimAutopilot { on } => {
            let n = own_claim(&state.fleet, c.viewer)
                .ok_or("claim_autopilot needs a GPU of your own (claim_gpu)")?;
            let now = state.fleet.scene.t_at(at);
            let claim = state
                .fleet
                .slots
                .get_mut(&n)
                .unwrap()
                .claim
                .as_mut()
                .unwrap();
            let intent = Some(c.id.to_string());
            match (*on, claim.autopilot) {
                (true, true) => Ok("autopilot already on".into()),
                (false, false) => Ok("autopilot already off".into()),
                (true, false) => {
                    crate::autopilot::resume(claim, now, intent);
                    Ok("autopilot on".into())
                }
                (false, true) => {
                    crate::autopilot::pause(claim, now, crate::autopilot::PAUSE_SECS, intent);
                    Ok("autopilot off".into())
                }
            }
        }
    }
}

fn own_claim(f: &Fleet, viewer: &str) -> Option<u32> {
    f.slots
        .iter()
        .find(|(_, s)| s.claim.as_ref().is_some_and(|c| c.viewer == viewer))
        .map(|(n, _)| *n)
}

fn claim(state: &mut State, c: &Ctx) -> Result<String, String> {
    let at = c.at;
    let cfg = state.config.clone();
    let f = &mut state.fleet;
    let token = c
        .secret
        .ok_or("a claim needs a session token from the coordinator")?;
    if f.slots
        .values()
        .any(|s| s.claim.as_ref().is_some_and(|k| k.client == c.client))
    {
        return Err("one GPU per client at a time".into());
    }
    if let Some(end) = f.last_claim_end.get(c.client) {
        let free_at = end + cfg.claim_cooldown_secs * 1000;
        if at < free_at {
            return Err(format!(
                "your last GPU was returned {} s ago; claim again in {} s",
                (at - end) / 1000,
                (free_at - at).div_ceil(1000)
            ));
        }
    }
    let hosts = f.hosts.clone();
    let quarantined = f.quarantined.clone();
    let usable = |s: &Slot| {
        s.claim.is_none()
            && s.host
                .as_ref()
                .and_then(|h| hosts.get(h))
                .is_some_and(|h| !h.draining)
            && !matches!((&s.host, &s.gpu), (Some(h), Some(g)) if quarantined.contains_key(&gpu_key(h, &g.sbdf)))
    };
    // A standby GPU when there is one; otherwise a wall tile is lent. Only a tile that is
    // running its current generation (not relaunching, not still racing) can be lent; the
    // last such tile in slot order goes.
    let standby = f
        .slots
        .iter()
        .find(|(_, s)| s.position.is_none() && usable(s))
        .map(|(n, _)| *n);
    let (n, borrowed) = match standby {
        Some(n) => (n, None),
        None => {
            let lent = f
                .slots
                .values()
                .filter(|s| s.claim.as_ref().is_some_and(|k| k.borrowed.is_some()))
                .count();
            let cap = cfg.borrow_cap(f.n);
            if lent >= cap {
                return Err(format!(
                    "{lent} of {cap} wall tiles are already lent to visitors; try again when one returns"
                ));
            }
            let racing = |id: &str| {
                f.races.iter().any(|r| {
                    r.fcvm.done_at.is_none()
                        && r.fcvm
                            .tiles
                            .get(id)
                            .is_some_and(|t| t.first_frame_at.is_none())
                })
            };
            let Some((n, k)) = f
                .slots
                .iter()
                .filter(|(_, s)| {
                    usable(s)
                        && s.observed.phase == "running"
                        && s.observed.generation == s.generation
                        && !racing(&s.id)
                })
                .filter_map(|(n, s)| Some((*n, s.position?)))
                .next_back()
            else {
                return Err(
                    "no wall tile can be lent now: every tile is relaunching or racing".into(),
                );
            };
            (n, Some(format!("fcvm:{}", k + 1)))
        }
    };
    f.claim_count += 1;
    let session = format!("claim-{}", f.claim_count);
    // The camera waits at the autopilot's start until the first frame shows, then flies.
    let start = crate::autopilot::start_pose(f.scene.t_at(at), Some(c.id.to_string()));
    let slot = f.slots.get_mut(&n).unwrap();
    bump(slot, &format!("claim:{session}"), at);
    slot.claim = Some(Claim {
        session: session.clone(),
        viewer: c.viewer.to_string(),
        client: c.client.to_string(),
        token: token.to_string(),
        claimed_at: at,
        expires_at: at + cfg.claim_secs * 1000,
        generation: slot.generation,
        first_frame_at: None,
        camera: vec![start],
        autopilot: true,
        flight_t0: None,
        borrowed,
    });
    Ok(format!("{session} on {}", slot.id))
}

/// End a claim: the slot is relaunched clean, back to standby.
fn release(f: &mut Fleet, n: u32, at: u64) {
    let Some(slot) = f.slots.get_mut(&n) else {
        return;
    };
    let Some(claim) = slot.claim.take() else {
        return;
    };
    // The VM restores back into the wall (or to standby), timed like any relaunch.
    bump(slot, &format!("return:{}", claim.session), at);
    f.last_claim_end.insert(claim.client, at);
}

fn host_draining(hosts: &BTreeMap<String, Host>, slot: &Slot) -> bool {
    slot.host
        .as_ref()
        .and_then(|h| hosts.get(h))
        .is_some_and(|h| h.draining)
}

fn bump(slot: &mut Slot, by: &str, at: u64) {
    slot.generation += 1;
    slot.last_relaunch = Some(Relaunch {
        generation: slot.generation,
        clicked_at: at,
        by: by.to_string(),
        first_frame_at: None,
        phases: Default::default(),
    });
}

/// A race: every tile relaunches at once (draining hosts and claimed GPUs excepted), and
/// every placed tile is timed. Returns the race id.
fn start_race(state: &mut State, by: &str, at: u64) -> u64 {
    let cfg = &state.config;
    let f = &mut state.fleet;
    let hosts = f.hosts.clone();
    let mut race = Race {
        id: f.race_count + 1,
        started_at: at,
        by: by.to_string(),
        fcvm: HalfRace::default(),
        ec2: HalfRace::default(),
    };
    for slot in f.slots.values_mut() {
        if let (Some(c), Some(k)) = (&slot.claim, slot.position) {
            if c.borrowed.is_some() {
                race.fcvm.sitting_out.push(format!("fcvm:{}", k + 1));
            }
        }
        if host_draining(&hosts, slot) || slot.claim.is_some() {
            continue;
        }
        bump(slot, by, at);
        if let Some(k) = slot.position {
            race.fcvm.tiles.insert(
                slot.id.clone(),
                RaceTile {
                    tile: format!("fcvm:{}", k + 1),
                    generation: slot.generation,
                    first_frame_at: None,
                    phases: BTreeMap::new(),
                    capacity_wait_ms: 0,
                    waiting_since: None,
                },
            );
        }
    }
    // Every EC2 tile races, also one still waiting for capacity (that wait is kept out of
    // the half's time); a tile without an instance has no place yet and names its slot.
    for slot in f.ec2.values_mut() {
        bump(slot, by, at);
        {
            race.ec2.tiles.insert(
                slot.id.clone(),
                RaceTile {
                    tile: slot
                        .position
                        .map(|k| format!("ec2:{}", k + 1))
                        .unwrap_or_else(|| slot.id.clone()),
                    generation: slot.generation,
                    first_frame_at: None,
                    phases: BTreeMap::new(),
                    capacity_wait_ms: 0,
                    waiting_since: None,
                },
            );
        }
    }
    f.race_count = race.id;
    f.races.push_back(race);
    while f.races.len() > MAX_RACES {
        f.races.pop_front();
    }
    f.cooldown_until = Some(at + cfg.restart_cooldown_secs * 1000);
    if let Some(s) = cfg.race_interval_secs {
        f.next_race_at = Some(at + s * 1000);
    }
    f.race_count
}

/// Keep the latest `MAX_WORDS` viewer words. A system word is never counted or evicted
/// here, so placing one cannot change the sky a viewer's text check was made against.
fn trim_viewer_words(words: &mut Vec<Word>) {
    while words.iter().filter(|w| !w.system).count() > MAX_WORDS {
        let oldest = words.iter().position(|w| !w.system).expect("a viewer word");
        words.remove(oldest);
    }
}

/// Write a line of GPU history into the scene when a viewer is watching an awake wall and
/// nothing has been in the sky for `history_idle_secs`. It goes in through the scene like
/// a viewer's word (no text check: the lines are ours), marked `system`, and takes no
/// action or receipt, so the feed credits nobody. A viewer's word, being newer, replaces it
/// and moves `history_ready_at` past its own hold.
fn history_step(state: &mut State, at: u64) {
    let gap = state.config.history_idle_secs * 1000;
    if gap == 0 {
        return;
    }
    let seed = state.config.camera_seed;
    let f = &mut state.fleet;
    let watched = f.power.state == "awake" && f.viewers.keys().any(|v| !f.hidden.contains(v));
    if !watched {
        // The gap starts when someone is there to see it.
        f.history_ready_at = Some(f.history_ready_at.map_or(at + gap, |r| r.max(at + gap)));
        return;
    }
    match f.history_ready_at {
        None => f.history_ready_at = Some(at + gap),
        Some(ready) if at >= ready => {
            let (text, text_style) = history::pick(seed, f.history_count);
            f.scene.words.retain(|w| !w.system);
            let t = f.scene.t_at(at);
            f.scene.words.push(Word {
                text: text.to_string(),
                t,
                text_style,
                system: true,
            });
            f.history_count += 1;
            f.history_ready_at = Some(at + history::hold_ms(text_style) + gap);
        }
        Some(_) => {}
    }
}

fn tick(state: &mut State, at: u64) {
    if state.fleet.power.state != "asleep" && state.fleet.next_race_at.is_some_and(|t| at >= t) {
        start_race(state, "schedule", at);
    }
    let f = &mut state.fleet;
    // The directed camera.
    let now_t = f.scene.t_at(at);
    f.director.tick(now_t);
    f.scene.time_of_day = f.director.time_of_day(now_t);
    let weather = f.director.weather();
    if f.scene.weather != weather {
        f.scene.weather = weather;
    }
    let cam = f.director.camera();
    if f.scene.camera != cam {
        f.scene.camera = cam;
    }
    history_step(state, at);
    let f = &mut state.fleet;
    // Flying claims get their next keyframes.
    for s in f.slots.values_mut() {
        if let Some(c) = s.claim.as_mut() {
            crate::autopilot::extend(c, now_t);
        }
    }
    // Claims expire.
    let expired: Vec<u32> = f
        .slots
        .iter()
        .filter(|(_, s)| s.claim.as_ref().is_some_and(|c| at >= c.expires_at))
        .map(|(n, _)| *n)
        .collect();
    for n in expired {
        release(f, n, at);
    }
    // Borrowed phones come back; actions the agents have had time to carry out are dropped.
    let expired: Vec<u32> = f
        .phones
        .iter()
        .filter(|(_, s)| phone_claim(s).is_some_and(|c| at >= c.expires_at))
        .map(|(n, _)| *n)
        .collect();
    for n in expired {
        release_phone(f, n, at);
    }
    for s in f.phones.values_mut() {
        if let Some(p) = s.phone.as_mut() {
            p.actions.retain(|a| {
                let keep = if a.action == crate::phone::PhoneAction::Tap {
                    crate::phone::TAP_KEEP_MS
                } else {
                    crate::phone::ACTION_KEEP_MS
                };
                at < a.at + keep && a.generation == s.generation
            });
        }
    }
    // Borrowed sandboxes come back; actions the agents have had time to carry out are dropped.
    expire_windows(f, at);
    for s in f.windows.values_mut() {
        if let Some(w) = s.windows.as_mut() {
            w.actions.retain(|a| {
                at < a.at + crate::windows::ACTION_KEEP_MS && a.generation == s.generation
            });
        }
    }
    // Borrowed servers come back.
    expire_minecraft(f, at);
    // A reclaimed host is listed for a minute after it is gone.
    let hosts = f.hosts.clone();
    f.reclaimed.retain(|h, (_, gone)| {
        hosts.contains_key(h) || gone.is_some_and(|g| at < g + RECLAIMED_KEEP_MS)
    });
    // Per-client limits older than any interval are forgotten.
    let keep = |m: &mut BTreeMap<String, u64>, ms: u64| m.retain(|_, t| at < *t + ms);
    keep(&mut f.last_word_at, state.config.word_interval_ms);
    keep(&mut f.last_light_at, state.config.light_interval_ms);
    keep(&mut f.last_relaunch_at, state.config.relaunch_interval_ms);
    keep(
        &mut f.last_claim_end,
        state.config.claim_cooldown_secs * 1000,
    );
    keep(&mut f.last_phones_zoom_at, crate::phone::ZOOM_INTERVAL_MS);
    keep(
        &mut f.last_phone_claim_end,
        state.config.phone_claim_cooldown_secs * 1000,
    );
    keep(
        &mut f.last_windows_reset_at,
        state.config.relaunch_interval_ms,
    );
    keep(
        &mut f.last_windows_reset_all_at,
        crate::windows::RESET_ALL_INTERVAL_MS,
    );
    keep(
        &mut f.last_windows_claim_end,
        state.config.windows_claim_cooldown_secs * 1000,
    );
    keep(
        &mut f.last_minecraft_reset_at,
        state.config.relaunch_interval_ms,
    );
    keep(
        &mut f.last_minecraft_reset_all_at,
        crate::minecraft::RESET_ALL_INTERVAL_MS,
    );
    keep(
        &mut f.last_minecraft_fork_at,
        crate::minecraft::FORK_INTERVAL_MS,
    );
    keep(
        &mut f.last_minecraft_claim_end,
        state.config.minecraft_claim_cooldown_secs * 1000,
    );
    // Restores per minute are kept for the last hour (this minute and the 59 before).
    let minute = at / 60_000;
    while f
        .counters
        .restores_by_minute
        .front()
        .is_some_and(|(m, _)| *m + 60 <= minute)
    {
        f.counters.restores_by_minute.pop_front();
    }
}

fn hello(f: &mut Fleet, host: &str, arch: &str, gpus: &[Gpu], capacity: usize, at: u64) {
    // One slot per GPU address; a repeated address is ignored. At most `capacity` slots.
    let mut seen = std::collections::BTreeSet::new();
    let gpus: Vec<Gpu> = gpus
        .iter()
        .filter(|g| seen.insert(g.sbdf.clone()))
        .take(capacity)
        .cloned()
        .collect();
    f.slots.retain(|_, s| {
        s.host.as_deref() != Some(host)
            || gpus
                .iter()
                .any(|g| Some(&g.sbdf) == s.gpu.as_ref().map(|x| &x.sbdf))
    });
    for g in &gpus {
        let exists = f
            .slots
            .values()
            .any(|s| s.host.as_deref() == Some(host) && s.gpu.as_ref() == Some(g));
        if exists {
            continue;
        }
        // Slot names are reused lowest first.
        let n = (1..).find(|n| !f.slots.contains_key(n)).unwrap();
        f.slots.insert(
            n,
            new_slot(
                slot_name(n),
                Half::Fcvm,
                Some(host.to_string()),
                Some(g.clone()),
                at,
            ),
        );
    }
    f.hosts.insert(
        host.to_string(),
        Host {
            name: host.to_string(),
            arch: arch.to_string(),
            gpus,
            capacity,
            draining: false,
            joined_at: at,
            region: None,
            aggregate_url: None,
            aggregate_ipv6: None,
            aggregate_ipv4: None,
            aggregate_v4_port: None,
            aggregate_published: None,
            phone_actions: Vec::new(),
            windows_actions: Vec::new(),
            windows_image: None,
            minecraft_server: None,
        },
    );
    f.reclaimed.remove(host);
    drop_missing_from_races(f, at);
}

fn new_slot(id: String, half: Half, host: Option<String>, gpu: Option<Gpu>, at: u64) -> Slot {
    Slot {
        id,
        half,
        host,
        gpu,
        generation: 1,
        created_at: at,
        position: None,
        rect: None,
        previous_rect: None,
        rect_generation: 0,
        observed: Observed {
            phase: "pending".into(),
            ..Default::default()
        },
        published: None,
        last_relaunch: None,
        claim: None,
        phone: None,
        windows: None,
        minecraft: None,
    }
}

/// What a slot report says about the slot's launch, for a wall slot, an EC2 tile or a phone.
fn note_report(slot: &mut Slot, r: &SlotReport, at: u64) {
    slot.observed.phase = r.phase.clone();
    slot.observed.generation = r.generation;
    slot.observed.vm = r.vm.clone();
    slot.observed.frame_url = r.frame_url.clone();
    slot.observed.timings = r.timings.clone();
    slot.observed.error = r.error.clone();
    slot.observed.retry_at = r.retry_in_ms.map(|ms| at + ms);
    slot.observed.ipv6 = r.ipv6.clone();
    slot.observed.ipv4 = r.ipv4.clone();
    slot.observed.v4_port = r.v4_port;
    if r.place.is_some() {
        slot.observed.place = r.place.clone();
    }
}

/// A report that is not `running` clears the renderer's readings (false); a `running` one
/// ends the timing of the launch it reports (true).
fn note_running(slot: &mut Slot, r: &SlotReport, at: u64) -> bool {
    if r.phase != "running" {
        slot.observed.fps = None;
        slot.observed.gpu_stats = None;
        slot.observed.budget_fit = None;
        slot.observed.render_ms_max = None;
        slot.observed.budget_every_pixel = None;
        slot.observed.boot_id = None;
        slot.observed.frame_t = None;
        return false;
    }
    match slot.last_relaunch.as_mut() {
        Some(rl) => {
            if rl.generation == r.generation && rl.first_frame_at.is_none() {
                rl.first_frame_at = Some(at);
                rl.phases = r.timings.clone();
            }
        }
        // A slot that came up at a wake was never relaunched by anyone, so nothing timed
        // it: its launch (or resume) is the last one, timed by the slot's own report.
        None => {
            let took = r.timings.get("total_ms").copied().unwrap_or(0);
            slot.last_relaunch = Some(Relaunch {
                generation: r.generation,
                clicked_at: at.saturating_sub(took),
                by: "wake".into(),
                first_frame_at: Some(at),
                phases: r.timings.clone(),
            });
        }
    }
    true
}

fn observe_slot(f: &mut Fleet, half: Half, n: u32, host: Option<&str>, r: &SlotReport, at: u64) {
    // A fcvm VM that reaches `running` from any other phase is one restore.
    if half == Half::Fcvm && r.phase == "running" {
        let restored = f.slots.get(&n).is_some_and(|s| {
            (host.is_none() || s.host.as_deref() == host) && s.observed.phase != "running"
        });
        if restored {
            count_restore(&mut f.counters, at);
        }
    }
    let slot = match half {
        Half::Fcvm => f.slots.get_mut(&n),
        Half::Ec2 => f.ec2.get_mut(&n),
    };
    let Some(slot) = slot else { return };
    if host.is_some() && slot.host.as_deref() != host {
        return;
    }
    // An agent that reconnected with its VMs running reports generations this slot (made
    // again at its hello) has not reached; adopt them, so a relaunch goes past them.
    if half == Half::Fcvm && r.generation > slot.generation {
        slot.generation = r.generation;
    }
    note_report(slot, r, at);
    if r.phase == "waiting_capacity" {
        let id = slot.id.clone();
        let since = at.saturating_sub(r.timings.get("capacity_wait_ms").copied().unwrap_or(0));
        for race in f.races.iter_mut() {
            let h = match half {
                Half::Fcvm => &mut race.fcvm,
                Half::Ec2 => &mut race.ec2,
            };
            if let Some(t) = h.tiles.get_mut(&id) {
                if h.done_at.is_none() && t.first_frame_at.is_none() && r.generation >= t.generation
                {
                    t.waiting_since.get_or_insert(since);
                }
            }
        }
    }
    let Some(slot) = (match half {
        Half::Fcvm => f.slots.get_mut(&n),
        Half::Ec2 => f.ec2.get_mut(&n),
    }) else {
        return;
    };
    if !note_running(slot, r, at) {
        return;
    }
    let scene_now = f.scene.t_at(at);
    if let Some(c) = slot.claim.as_mut() {
        if c.generation == r.generation && c.first_frame_at.is_none() {
            c.first_frame_at = Some(at);
            crate::autopilot::start(c, scene_now + crate::autopilot::START_LEAD_SECS, scene_now);
        }
    }
    let id = slot.id.clone();
    // The running report finishes this tile in every race still waiting for it.
    for race in f.races.iter_mut() {
        let h = match half {
            Half::Fcvm => &mut race.fcvm,
            Half::Ec2 => &mut race.ec2,
        };
        if h.done_at.is_some() {
            continue;
        }
        let Some(t) = h.tiles.get_mut(&id) else {
            continue;
        };
        if t.first_frame_at.is_some() || r.generation < t.generation {
            continue;
        }
        t.first_frame_at = Some(at);
        t.phases = r.timings.clone();
        t.capacity_wait_ms = r.timings.get("capacity_wait_ms").copied().unwrap_or(0);
        finish_half(h, race.started_at, Some(id.clone()), at);
    }
}

/// A half is done when every tile still in it has its first frame: its time is the slowest
/// tile's click to first frame, less that tile's capacity wait.
fn finish_half(h: &mut HalfRace, started_at: u64, last: Option<String>, at: u64) {
    if h.done_at.is_some() || h.tiles.is_empty() {
        return;
    }
    if h.tiles.values().any(|t| t.first_frame_at.is_none()) {
        return;
    }
    h.done_at = Some(at);
    h.last = last;
    h.time_ms = h
        .tiles
        .values()
        .map(|t| (t.first_frame_at.unwrap() - started_at).saturating_sub(t.capacity_wait_ms))
        .max();
    h.capacity_wait_ms = h
        .tiles
        .values()
        .map(|t| t.capacity_wait_ms)
        .max()
        .unwrap_or(0);
}

fn health(f: &mut Fleet, host: Option<&str>, h: &HealthReport) {
    let slot = match host {
        Some(_) if h.id.starts_with("phone-") => {
            phone_number(&h.id).and_then(|n| f.phones.get_mut(&n))
        }
        Some(_) => slot_number(&h.id).and_then(|n| f.slots.get_mut(&n)),
        None => ec2_number(&h.id).and_then(|n| f.ec2.get_mut(&n)),
    };
    let Some(slot) = slot else {
        return;
    };
    if (host.is_some() && slot.host.as_deref() != host) || slot.observed.phase != "running" {
        return;
    }
    slot.observed.fps = h.fps.is_finite().then_some(h.fps);
    // An EC2 tile has no inventory to name its GPU: the renderer says which one it drew on.
    if host.is_none() {
        if let Some(name) = h
            .gpu
            .get("name")
            .and_then(|n| n.as_str())
            .map(str::trim)
            .filter(|n| !n.is_empty())
        {
            if slot.gpu.as_ref().map(|g| g.name.as_str()) != Some(name) {
                slot.gpu = Some(Gpu {
                    sbdf: String::new(),
                    name: name.to_string(),
                });
            }
        }
    }
    slot.observed.gpu_stats = Some(h.gpu.clone());
    slot.observed.budget_fit = h.budget_fit.filter(|v| v.is_finite());
    slot.observed.render_ms_max = h.render_ms_max.filter(|v| v.is_finite());
    slot.observed.budget_every_pixel = h.budget_every_pixel.filter(|v| v.is_finite() && *v > 0.0);
    slot.observed.boot_id = h.boot_id.clone().filter(|b| !b.is_empty());
    slot.observed.frame_t = h.t.filter(|v| v.is_finite());
}

/// One budget round over every placed tile that draws its part of the wall (a claimed GPU
/// draws the whole image and adapts itself; an EC2 tile without an instance has no
/// renderer).
fn budget_round(f: &mut Fleet, at: u64) {
    if at < f.budget.next_round_at {
        return;
    }
    f.budget.next_round_at = at + crate::budget::ROUND_MS;
    let tiles: Vec<&Slot> = f
        .slots
        .values()
        .chain(f.ec2.values())
        .filter(|s| renders_on_wall(s) && s.claim.is_none())
        .collect();
    let fits: Vec<Option<f64>> = tiles.iter().map(|s| s.observed.budget_fit).collect();
    let over = |s: &Slot| {
        s.observed
            .render_ms_max
            .is_some_and(|ms| ms > crate::budget::OVERLOAD_MS)
    };
    let overloaded = tiles.iter().any(|s| over(s));
    let newest = tiles
        .iter()
        .filter_map(|s| s.observed.frame_t)
        .fold(0.0, f64::max);
    let observed: Vec<crate::budget::TileFit> = tiles
        .iter()
        .map(|s| crate::budget::TileFit {
            id: &s.id,
            // the renderer process a grant is given to, and posted to only (BudgetPlan::for_tile)
            owner: s.observed.boot_id.clone().unwrap_or_default(),
            fit: s.observed.budget_fit,
            every_pixel_budget: s.observed.budget_every_pixel,
            overloaded: over(s),
        })
        .collect();
    f.budget.round(&fits, overloaded, newest);
    f.budget.strides(&observed, newest);
}

/// Races stop waiting for tiles that leave (their host is reclaimed); a half that then has
/// every remaining tile finished is done now.
fn drop_from_races(f: &mut Fleet, leaving: impl Fn(&Slot) -> bool, at: u64) {
    let gone: Vec<String> = f
        .slots
        .values()
        .filter(|s| leaving(s))
        .map(|s| s.id.clone())
        .collect();
    for race in f.races.iter_mut() {
        if race.fcvm.done_at.is_some() {
            continue;
        }
        for id in &gone {
            if race
                .fcvm
                .tiles
                .get(id)
                .is_some_and(|t| t.first_frame_at.is_none())
            {
                race.fcvm.tiles.remove(id);
                race.fcvm.dropped += 1;
            }
        }
        let last = race.fcvm.last.clone();
        finish_half(&mut race.fcvm, race.started_at, last, at);
    }
}

fn drop_missing_from_races(f: &mut Fleet, at: u64) {
    let slots: Vec<String> = f.slots.values().map(|s| s.id.clone()).collect();
    let ec2: Vec<String> = f.ec2.values().map(|s| s.id.clone()).collect();
    for race in f.races.iter_mut() {
        for (h, live) in [(&mut race.fcvm, &slots), (&mut race.ec2, &ec2)] {
            if h.done_at.is_some() {
                continue;
            }
            let before = h.tiles.len();
            h.tiles
                .retain(|id, t| live.contains(id) || t.first_frame_at.is_some());
            h.dropped += before - h.tiles.len();
            let last = h.last.clone();
            finish_half(h, race.started_at, last, at);
        }
    }
}

/// Recompute each side's places and every slot's rectangle. Deterministic in the state, so
/// it runs after every event.
///
/// - fcvm GPUs: slots whose host is present and not draining, in slot-number order, at
///   most `max_tiles`; a GPU claimed from standby is left out, a borrowed wall tile keeps
///   its place. fcvm slots beyond `max_tiles` are standby.
/// - EC2 tiles wanted: `min(ec2_max, max_tiles)`; `ec2-1..ec2-D` exist, higher ones are
///   dropped (the driver terminates their instances). Each has a place, instance or not.
/// - fcvm places: `max(min(fcvm_tiles, max_tiles) - phone GPUs - quarantined GPUs, fcvm
///   GPUs)`, where phone GPUs are those connected hosts run phones on and quarantined GPUs
///   are wall GPUs of connected hosts out of rotation: no GPU can fill either's place. Once
///   every host the launcher keeps (`expected_hosts`) has connected and none is draining,
///   the first term is at most the GPUs the wall could place, so a GPU no hello listed
///   leaves no place. The
///   GPUs are grouped into one band per host and the places left over are one dark band
///   below them.
/// - Asleep, neither side has a place.
fn retile(f: &mut Fleet, c: &Config, at: u64) {
    // Asleep, nothing is placed: the fcvm side's GPUs go to standby (their VMs stop) as the
    // EC2 side's tiles go away, whether or not their hosts scale down with them.
    let asleep = f.power.state == "asleep";
    let mut wall: Vec<u32> = f
        .slots
        .iter()
        .filter(|_| !asleep)
        .filter(|(_, s)| {
            s.claim.as_ref().is_none_or(|c| c.borrowed.is_some())
                && !is_quarantined(f, s)
                && s.host
                    .as_ref()
                    .and_then(|h| f.hosts.get(h))
                    .is_some_and(|h| !h.draining)
        })
        .map(|(n, _)| *n)
        .collect();
    // A GPU joins the wall only once its VM renders (it warms first, drawing the whole image
    // off the wall), so a re-tile never waits on a VM that is still starting. While none of
    // the side is on the wall (a cold start, a wake) every GPU joins at once.
    let placed = |n: &u32| f.slots[n].position.is_some();
    let any_placed = wall.iter().any(placed);
    // The GPUs the wall could place: once every host the launcher keeps has connected, no
    // other GPU is coming, so a place beyond these would stay dark (#1517).
    let eligible = wall.len().min(c.max_tiles);
    let beyond = wall.split_off(wall.len().min(c.max_tiles));
    let (mut active, warming): (Vec<u32>, Vec<u32>) = wall.into_iter().partition(|n| {
        let s = &f.slots[n];
        !any_placed
            || s.position.is_some()
            || (s.observed.phase == "running" && s.observed.generation >= s.generation)
    });
    // A tile already on the wall keeps its place while a GPU that would displace it (a lower
    // slot back from quarantine) warms; it goes to standby once that one joins.
    for n in beyond {
        if active.len() >= c.max_tiles {
            break;
        }
        if placed(&n) {
            active.push(n);
        }
    }
    active.sort_unstable();
    let was_warming = std::mem::replace(&mut f.warming, warming.into_iter().collect());
    // The EC2 side wants its configured tiles whatever the fcvm side has (each side shows
    // the GPUs it has live); asleep it has none.
    let want_ec2 = if asleep {
        0
    } else {
        c.ec2_max.min(c.max_tiles) as u32
    };
    f.ec2.retain(|n, _| *n <= want_ec2);
    for n in 1..=want_ec2 {
        f.ec2
            .entry(n)
            .or_insert_with(|| new_slot(ec2_name(n), Half::Ec2, None, None, at));
    }
    // Each side is as wide as its share of the configured places, so a tile going dark (a
    // race replacing the EC2 instances, a capacity wait, a host not yet connected or
    // reclaimed) re-tiles nothing. Every EC2 tile has its place, with an instance or not;
    // the fcvm side has `fcvm_tiles` places less the GPUs connected hosts run phones on
    // (never wall tiles) and less the wall GPUs of connected hosts that are quarantined (out
    // until an operator releases them), or one per live GPU when there are more. A place
    // kept for either would stay dark for as long as its host runs.
    let ec2_places: Vec<u32> = f.ec2.keys().copied().take(c.max_tiles).collect();
    let quarantined = f
        .slots
        .values()
        .filter(|s| is_quarantined(f, s) && !host_draining(&f.hosts, s))
        .filter(|s| s.host.as_ref().is_some_and(|h| f.hosts.contains_key(h)))
        .count();
    // Connected and staying: a draining host's places are held for its replacement.
    let all_connected = f.expected_hosts.as_ref().is_some_and(|ids| {
        !ids.is_empty()
            && ids
                .iter()
                .all(|id| f.hosts.get(id).is_some_and(|h| !h.draining))
    });
    let fcvm_places = if asleep {
        0
    } else {
        let configured = c
            .fcvm_tiles
            .min(c.max_tiles)
            .saturating_sub(f.phones.len())
            .saturating_sub(quarantined);
        let configured = if all_connected {
            configured.min(eligible)
        } else {
            configured
        };
        configured.max(active.len())
    };
    f.n = active.len();
    f.ec2_n = ec2_places.len();
    let (fcvm_side, ec2_side) = sides(fcvm_places, f.ec2_n);
    // fcvm: one band per host, hosts ordered by their lowest placed slot.
    let mut groups: Vec<(String, Vec<u32>)> = Vec::new();
    for num in &active {
        let host = f.slots[num].host.clone().expect("active slots have a host");
        match groups.iter_mut().find(|(h, _)| *h == host) {
            Some((_, v)) => v.push(*num),
            None => groups.push((host, vec![*num])),
        }
    }
    // The places with no GPU are one band below the hosts'.
    let mut counts: Vec<usize> = groups.iter().map(|(_, v)| v.len()).collect();
    let dark = fcvm_places - f.n;
    if dark > 0 {
        counts.push(dark);
    }
    let host_bands = if fcvm_places == 0 {
        Vec::new()
    } else {
        bands(fcvm_side, &counts, c.image_aspect, c.tile_aspect)
    };
    let fcvm_dark: Vec<Rect> = if dark > 0 {
        host_bands.last().expect("a band per count").1.clone()
    } else {
        Vec::new()
    };
    let ec2_rects = grid_in(ec2_side, f.ec2_n, c.image_aspect, c.tile_aspect);
    // The new place of every slot, then compare with the old.
    let mut placed: BTreeMap<(Half, u32), (usize, Rect)> = BTreeMap::new();
    for h in f.hosts.values_mut() {
        h.region = None;
    }
    let mut k = 0;
    for (gi, (host, nums)) in groups.iter().enumerate() {
        if let Some(h) = f.hosts.get_mut(host) {
            h.region = Some(host_bands[gi].0);
        }
        for (i, num) in nums.iter().enumerate() {
            placed.insert((Half::Fcvm, *num), (k, host_bands[gi].1[i]));
            k += 1;
        }
    }
    for (i, num) in ec2_places.iter().enumerate() {
        placed.insert((Half::Ec2, *num), (i, ec2_rects[i]));
    }
    let changed = f
        .slots
        .iter()
        .map(|(n, s)| ((Half::Fcvm, *n), s))
        .chain(f.ec2.iter().map(|(n, s)| ((Half::Ec2, *n), s)))
        .any(|(key, s)| placed.get(&key).map(|p| (p.0, p.1)) != s.position.zip(s.rect))
        // A slot that left moved nothing still here, but the sides changed.
        || f.fcvm_dark != fcvm_dark
        || f.layout.fcvm != (fcvm_places > 0).then_some(fcvm_side)
        || f.layout.ec2 != (f.ec2_n > 0).then_some(ec2_side);
    f.fcvm_dark = fcvm_dark;
    let generation = if changed {
        f.layout.generation + 1
    } else {
        f.layout.generation
    };
    if changed {
        f.layout = LayoutGen {
            generation,
            from_frame: crate::state::layout_from_frame(f.scene.t_at(at)),
            fcvm: (fcvm_places > 0).then_some(fcvm_side),
            ec2: (f.ec2_n > 0).then_some(ec2_side),
        };
    }
    for (half, slots) in [(Half::Fcvm, &mut f.slots), (Half::Ec2, &mut f.ec2)] {
        for (n, s) in slots.iter_mut() {
            let new = placed.get(&(half, *n)).copied();
            let (pos, rect) = (new.map(|p| p.0), new.map(|p| p.1));
            if rect != s.rect {
                // A GPU that warmed drew the whole image until it joins.
                s.previous_rect = match (half, s.rect) {
                    (Half::Fcvm, None) if was_warming.contains(n) => Some(Rect {
                        x: 0.0,
                        y: 0.0,
                        w: 1.0,
                        h: 1.0,
                    }),
                    _ => s.rect,
                };
                s.rect_generation = generation;
            }
            s.position = pos;
            s.rect = rect;
        }
    }
    drop_missing_from_races(f, at);
}

// ---------------------------------------------------------------------------
// Phones mode
// ---------------------------------------------------------------------------

fn phone_claim(s: &Slot) -> Option<&crate::phone::PhoneClaim> {
    s.phone.as_ref().and_then(|p| p.claim.as_ref())
}

/// The phones of a host's hello: one per phone GPU (not one of its wall GPUs, and an
/// address once), the host's others removed.
fn phone_hello(f: &mut Fleet, host: &str, gpus: &[Gpu], phones: &[Gpu], at: u64) {
    let mut seen: std::collections::BTreeSet<&str> = gpus.iter().map(|g| g.sbdf.as_str()).collect();
    let phones: Vec<&Gpu> = phones.iter().filter(|g| seen.insert(&g.sbdf)).collect();
    f.phones.retain(|_, s| {
        s.host.as_deref() != Some(host)
            || phones
                .iter()
                .any(|g| Some(&g.sbdf) == s.gpu.as_ref().map(|x| &x.sbdf))
    });
    for g in phones {
        let exists = f
            .phones
            .values()
            .any(|s| s.host.as_deref() == Some(host) && s.gpu.as_ref() == Some(g));
        if exists {
            continue;
        }
        let n = (1..).find(|n| !f.phones.contains_key(n)).unwrap();
        let mut s = new_slot(
            phone_name(n),
            Half::Fcvm,
            Some(host.to_string()),
            Some(g.clone()),
            at,
        );
        s.phone = Some(Default::default());
        f.phones.insert(n, s);
    }
}

fn observe_phone(f: &mut Fleet, n: u32, host: &str, r: &SlotReport, at: u64) {
    let Some(slot) = f.phones.get_mut(&n) else {
        return;
    };
    if slot.host.as_deref() != Some(host) {
        return;
    }
    if r.phase == "running" && slot.observed.phase != "running" {
        count_restore(&mut f.counters, at);
    }
    if r.generation > slot.generation {
        slot.generation = r.generation;
    }
    note_report(slot, r, at);
    note_running(slot, r, at);
}

fn own_phone(f: &Fleet, viewer: &str) -> Option<u32> {
    f.phones
        .iter()
        .find(|(_, s)| phone_claim(s).is_some_and(|c| c.viewer == viewer))
        .map(|(n, _)| *n)
}

fn check_phone_claim(f: &Fleet, n: u32, claim: Option<&str>) -> Result<(), String> {
    let session = &phone_claim(&f.phones[&n]).unwrap().session;
    match claim {
        Some(k) if k != session => Err(format!("{k:?} is not this viewer's phone")),
        _ => Ok(()),
    }
}

/// Phones a viewer may borrow: placed on a host that is not draining, on a GPU not in
/// quarantine, running its current launch, not lent.
fn phone_free(f: &Fleet, s: &Slot) -> Result<(), String> {
    if phone_claim(s).is_some() {
        return Err(format!("{} is lent to another viewer", s.id));
    }
    if host_draining(&f.hosts, s) || is_quarantined(f, s) {
        return Err(format!("{} is leaving the fleet", s.id));
    }
    if s.observed.phase != "running" || s.observed.generation < s.generation {
        return Err(format!("{} is not running", s.id));
    }
    Ok(())
}

fn claim_phone(state: &mut State, c: &Ctx, want: Option<&str>) -> Result<String, String> {
    let at = c.at;
    let cfg = &state.config;
    let f = &mut state.fleet;
    if f.phones
        .values()
        .any(|s| phone_claim(s).is_some_and(|k| k.client == c.client))
    {
        return Err("one phone per client at a time".into());
    }
    if let Some(end) = f.last_phone_claim_end.get(c.client) {
        let free_at = end + cfg.phone_claim_cooldown_secs * 1000;
        if at < free_at {
            return Err(format!(
                "your last phone was returned {} s ago; borrow again in {} s",
                (at - end) / 1000,
                (free_at - at).div_ceil(1000)
            ));
        }
    }
    let lent: Vec<u64> = f
        .phones
        .values()
        .filter_map(phone_claim)
        .map(|k| k.expires_at)
        .collect();
    let cap = cfg.phone_lend_cap(f.phones.len());
    if lent.len() >= cap {
        let back = lent
            .iter()
            .min()
            .map_or(0, |t| t.saturating_sub(at).div_ceil(1000));
        return Err(format!(
            "{} of {cap} phones are already lent; the next comes back in {back} s",
            lent.len()
        ));
    }
    let n = match want {
        Some(id) => {
            let n = phone_number(id)
                .filter(|n| f.phones.contains_key(n))
                .ok_or_else(|| format!("no phone {id:?}"))?;
            phone_free(f, &f.phones[&n])?;
            n
        }
        None => *f
            .phones
            .iter()
            .find(|(_, s)| phone_free(f, s).is_ok())
            .map(|(n, _)| n)
            .ok_or("no phone can be lent now: none is running and free")?,
    };
    f.phone_claim_count += 1;
    let session = format!("phone-claim-{}", f.phone_claim_count);
    let slot = f.phones.get_mut(&n).unwrap();
    slot.phone.get_or_insert_with(Default::default).claim = Some(crate::phone::PhoneClaim {
        session: session.clone(),
        viewer: c.viewer.to_string(),
        client: c.client.to_string(),
        claimed_at: at,
        expires_at: at + cfg.phone_claim_secs * 1000,
        last_action_at: None,
        last_tap_at: None,
    });
    Ok(format!("{session} on {}", slot.id))
}

/// Give a phone back: it is restored from its snapshot, so the next viewer finds it as
/// every phone starts (not rotated, not dark, the app on its scene).
fn release_phone(f: &mut Fleet, n: u32, at: u64) {
    let Some(slot) = f.phones.get_mut(&n) else {
        return;
    };
    let Some(claim) = slot.phone.as_mut().and_then(|p| p.claim.take()) else {
        return;
    };
    bump(slot, &format!("return:{}", claim.session), at);
    f.last_phone_claim_end.insert(claim.client, at);
}

/// The take-over actions phone `n` offers: those its host's hello listed.
pub fn phone_offers(f: &Fleet, n: u32) -> Vec<crate::phone::PhoneAction> {
    let listed = f
        .phones
        .get(&n)
        .and_then(|s| s.host.as_ref())
        .and_then(|h| f.hosts.get(h))
        .map(|h| h.phone_actions.as_slice())
        .unwrap_or_default();
    crate::phone::PhoneAction::offered(listed)
}

fn phone_action(
    state: &mut State,
    c: &Ctx,
    claim: Option<&str>,
    action: crate::phone::PhoneAction,
    point: Option<crate::phone::TapPoint>,
) -> Result<String, String> {
    use crate::phone::{Front, PhoneAction};
    let at = c.at;
    let f = &mut state.fleet;
    let n = own_phone(f, c.viewer).ok_or("phone actions need a phone of your own (claim_phone)")?;
    check_phone_claim(f, n, claim)?;
    // Only what the phone's host carries out: an agent skips the whole assignment line that
    // names an action it does not know.
    if !phone_offers(f, n).contains(&action) {
        return Err(format!(
            "{}'s host does not support {}",
            f.phones[&n].id,
            action.as_str()
        ));
    }
    let slot = f.phones.get_mut(&n).unwrap();
    let generation = slot.generation;
    let running = slot.observed.phase == "running" && slot.observed.generation >= generation;
    let p = slot.phone.as_mut().unwrap();
    let front = p.front(generation);
    let k = p.claim.as_mut().unwrap();
    if action == PhoneAction::Tap {
        // A touch is for the screen the viewer sees: Robot Park in front, on a running phone.
        if !running {
            return Err(format!("{} is not running", slot.id));
        }
        if front == Front::Browser {
            return Err(format!(
                "taps reach the phone only in Robot Park; {} shows the browser",
                slot.id
            ));
        }
        if let Some(last) = k.last_tap_at {
            let next = last + crate::phone::TAP_INTERVAL_MS;
            if at < next {
                return Err(format!(
                    "one tap every {} ms; wait {} ms",
                    crate::phone::TAP_INTERVAL_MS,
                    next - at
                ));
            }
        }
        k.last_tap_at = Some(at);
    } else {
        if let Some(last) = k.last_action_at {
            let next = last + crate::phone::ACTION_INTERVAL_MS;
            if at < next {
                return Err(format!("one action a second; wait {} ms", next - at));
            }
        }
        k.last_action_at = Some(at);
    }
    let session = k.session.clone();
    if action == PhoneAction::RebootPhone {
        // A reboot restores the phone's VM, timed like any relaunch.
        bump(slot, &format!("reboot:{session}"), at);
        return Ok(format!("reboot {}", slot.id));
    }
    match action.front_after() {
        Some(Front::Browser) => p.browser_generation = Some(generation),
        Some(Front::App) => p.browser_generation = None,
        None => {}
    }
    f.phone_action_seq += 1;
    p.actions.push(crate::phone::PhoneActionReq {
        seq: f.phone_action_seq,
        action,
        at,
        generation,
        point,
    });
    Ok(match point {
        Some(t) => format!("tap at {:.3}, {:.3} on {}", t.x, t.y, slot.id),
        None => format!("{} on {}", action.as_str(), slot.id),
    })
}

/// The shared zoom: a chrome key every phone draws from a scene time a little ahead, so all
/// change together. A zoom already in effect commits and changes nothing.
fn phones_zoom(state: &mut State, c: &Ctx, out: bool) -> Result<String, String> {
    let at = c.at;
    let f = &mut state.fleet;
    if f.phones.is_empty() {
        return Err("this fleet runs no phones".into());
    }
    if f.phones_zoom.out == out {
        return Ok(if out {
            "already zoomed out"
        } else {
            "already zoomed in"
        }
        .into());
    }
    if let Some(last) = f.last_phones_zoom_at.get(c.client) {
        let next = last + crate::phone::ZOOM_INTERVAL_MS;
        if at < next {
            return Err(format!("one zoom every 2 s; wait {} ms", next - at));
        }
    }
    f.last_phones_zoom_at.insert(c.client.to_string(), at);
    let t = f.scene.t_at(at) + crate::phone::ZOOM_LEAD_SECS;
    let z = &mut f.phones_zoom;
    z.out = out;
    z.at = Some(at);
    z.keys.push(crate::phone::ChromeKey { t, on: out });
    let n = z.keys.len();
    if n > crate::phone::ZOOM_KEYS {
        z.keys.drain(..n - crate::phone::ZOOM_KEYS);
    }
    Ok(if out { "zoom out" } else { "zoom in" }.into())
}

// ---------------------------------------------------------------------------
// Windows mode
// ---------------------------------------------------------------------------

fn windows_claim(s: &Slot) -> Option<&crate::windows::WindowsClaim> {
    s.windows.as_ref().and_then(|w| w.claim.as_ref())
}

fn windows_index(s: &Slot) -> Option<u32> {
    s.windows.as_ref().map(|w| w.index)
}

/// The sandboxes of a host's hello: one per index below `count`, the host's others removed.
/// A sandbox keeps its name and launch across a hello that keeps its index.
fn windows_hello(f: &mut Fleet, host: &str, count: u32, at: u64) {
    f.windows.retain(|_, s| {
        s.host.as_deref() != Some(host) || windows_index(s).is_some_and(|i| i < count)
    });
    for index in 0..count {
        let exists = f
            .windows
            .values()
            .any(|s| s.host.as_deref() == Some(host) && windows_index(s) == Some(index));
        if exists {
            continue;
        }
        let n = (1..).find(|n| !f.windows.contains_key(n)).unwrap();
        let mut s = new_slot(
            windows_name(n),
            Half::Fcvm,
            Some(host.to_string()),
            None,
            at,
        );
        s.windows = Some(crate::windows::WindowsSlot {
            index,
            ..Default::default()
        });
        f.windows.insert(n, s);
    }
}

/// A sandbox's report. Its launch's first frame is the bridge's desktop time, on the host's
/// clock, kept between the click and this read so a skewed clock cannot move it outside them.
fn observe_windows(f: &mut Fleet, n: u32, host: &str, r: &SlotReport, at: u64) {
    let Some(slot) = f.windows.get_mut(&n) else {
        return;
    };
    if slot.host.as_deref() != Some(host) {
        return;
    }
    if r.phase == "running" && slot.observed.phase != "running" {
        count_restore(&mut f.counters, at);
    }
    if r.generation > slot.generation {
        slot.generation = r.generation;
    }
    // The host came back running a desktop that was lent: reset it before anyone borrows it,
    // and keep owing the reset until the fresh launch reports running.
    if let Some(key) = windows_index(slot).map(|i| crate::windows::key(host, i)) {
        if let Some(owed) = f.windows_to_reset.get_mut(&key) {
            match owed.generation {
                Some(g) if r.phase == "running" && r.generation >= g => {
                    f.windows_to_reset.remove(&key);
                }
                Some(g) if slot.generation >= g => {}
                _ => {
                    slot.generation = slot
                        .generation
                        .max(owed.generation.unwrap_or(0).saturating_sub(1));
                    bump(slot, &format!("return:{}", owed.session), at);
                    owed.generation = Some(slot.generation);
                }
            }
        }
    }
    let timed = slot
        .last_relaunch
        .as_ref()
        .is_some_and(|rl| rl.first_frame_at.is_some());
    let clicked = slot.last_relaunch.is_some();
    note_report(slot, r, at);
    if !note_running(slot, r, at) || timed {
        return;
    }
    let (Some(rl), Some(desktop)) = (slot.last_relaunch.as_mut(), r.desktop_at) else {
        return;
    };
    if rl.generation != r.generation || rl.first_frame_at != Some(at) {
        return;
    }
    let desktop = desktop.min(at);
    if clicked {
        rl.first_frame_at = Some(desktop.max(rl.clicked_at.min(at)));
    } else {
        // Nobody clicked (a launch at the host's start): the launch began `total_ms` before
        // its desktop, both on the host's clock, however late the report is read.
        let took = r.timings.get("total_ms").copied().unwrap_or(0);
        rl.clicked_at = desktop.saturating_sub(took);
        rl.first_frame_at = Some(desktop);
    }
}

/// End every borrow whose time is up at `at`. A tick does it, and so does every Windows
/// intent before it is judged, so a borrow ends at its deadline and not at the next tick.
fn expire_windows(f: &mut Fleet, at: u64) {
    let expired: Vec<u32> = f
        .windows
        .iter()
        .filter(|(_, s)| windows_claim(s).is_some_and(|c| at >= c.expires_at))
        .map(|(n, _)| *n)
        .collect();
    for n in expired {
        release_windows(f, n, at);
    }
}

/// Remove the sandboxes `gone` picks. A lent one's borrow ends here (its client's cooldown
/// starts), and it is marked to be reset when its host reports it again: the agent keeps
/// its VMs running across a lost connection, and a coordinator restart does not stop them.
/// One given back whose reset has not run yet is already marked (`release_windows`).
pub fn forget_windows(f: &mut Fleet, gone: impl Fn(&Slot) -> bool, at: u64) {
    for s in f.windows.values().filter(|s| gone(s)) {
        if let (Some(c), Some(host), Some(i)) = (windows_claim(s), &s.host, windows_index(s)) {
            f.last_windows_claim_end.insert(c.client.clone(), at);
            f.windows_to_reset.insert(
                crate::windows::key(host, i),
                crate::windows::ResetOwed {
                    session: c.session.clone(),
                    generation: None,
                },
            );
        }
    }
    f.windows.retain(|_, s| !gone(s));
}

fn own_windows(f: &Fleet, viewer: &str) -> Option<u32> {
    f.windows
        .iter()
        .find(|(_, s)| windows_claim(s).is_some_and(|c| c.viewer == viewer))
        .map(|(n, _)| *n)
}

fn check_windows_claim(f: &Fleet, n: u32, claim: Option<&str>) -> Result<(), String> {
    let session = &windows_claim(&f.windows[&n]).unwrap().session;
    match claim {
        Some(k) if k != session => Err(format!("{k:?} is not this viewer's sandbox")),
        _ => Ok(()),
    }
}

/// The sandbox named `id`.
fn windows_slot(f: &Fleet, id: &str) -> Result<u32, String> {
    windows_number(id)
        .filter(|n| f.windows.contains_key(n))
        .ok_or_else(|| format!("no sandbox {id:?}"))
}

/// Sandboxes a viewer may borrow: on a host that is not draining, running their current
/// launch, not lent.
fn windows_free(f: &Fleet, s: &Slot) -> Result<(), String> {
    if windows_claim(s).is_some() {
        return Err(format!("{} is lent to another viewer", s.id));
    }
    if host_draining(&f.hosts, s) {
        return Err(format!("{} is leaving the fleet", s.id));
    }
    if s.observed.phase != "running" || s.observed.generation < s.generation {
        return Err(format!("{} is not running", s.id));
    }
    Ok(())
}

/// Restore one sandbox, or every one, from the gold snapshot: its generation moves and the
/// launch is timed from this click.
fn windows_reset(state: &mut State, c: &Ctx, slot: Option<&str>) -> Result<String, String> {
    let at = c.at;
    if state.fleet.power.state == "asleep" {
        return Err("the hosts are asleep".into());
    }
    let f = &mut state.fleet;
    match slot {
        Some(id) => {
            let n = windows_slot(f, id)?;
            rate(
                &f.last_windows_reset_at,
                c.client,
                at,
                state.config.relaunch_interval_ms,
                "reset",
            )?;
            let hosts = f.hosts.clone();
            let s = f.windows.get_mut(&n).unwrap();
            if windows_claim(s).is_some() {
                return Err(format!("{} is lent to a viewer", s.id));
            }
            if host_draining(&hosts, s) {
                return Err(format!("{} is on a draining host", s.id));
            }
            bump(s, &format!("viewer:{}", c.viewer), at);
            f.last_windows_reset_at.insert(c.client.to_string(), at);
            Ok(format!("reset {}", s.id))
        }
        None => {
            rate(
                &f.last_windows_reset_all_at,
                c.client,
                at,
                crate::windows::RESET_ALL_INTERVAL_MS,
                "reset all",
            )?;
            let hosts = f.hosts.clone();
            let mut n = 0;
            for s in f.windows.values_mut() {
                if windows_claim(s).is_some() || host_draining(&hosts, s) {
                    continue;
                }
                bump(s, "reset_all", at);
                n += 1;
            }
            if n == 0 {
                return Err("no sandbox can be reset now".into());
            }
            f.last_windows_reset_all_at.insert(c.client.to_string(), at);
            f.last_reset_all = Some(crate::windows::ResetAll {
                clicked_at: at,
                by: format!("viewer:{}", c.viewer),
                sandboxes: n,
            });
            Ok(format!("reset all {n} sandboxes"))
        }
    }
}

fn claim_windows(state: &mut State, c: &Ctx, want: Option<&str>) -> Result<String, String> {
    let at = c.at;
    let cfg = &state.config;
    let f = &mut state.fleet;
    if f.windows
        .values()
        .any(|s| windows_claim(s).is_some_and(|k| k.client == c.client))
    {
        return Err("one sandbox per client at a time".into());
    }
    if let Some(end) = f.last_windows_claim_end.get(c.client) {
        let free_at = end + cfg.windows_claim_cooldown_secs * 1000;
        if at < free_at {
            return Err(format!(
                "your last sandbox was returned {} s ago; borrow again in {} s",
                (at - end) / 1000,
                (free_at - at).div_ceil(1000)
            ));
        }
    }
    let lent: Vec<u64> = f
        .windows
        .values()
        .filter_map(windows_claim)
        .map(|k| k.expires_at)
        .collect();
    let cap = cfg.windows_lend_cap(f.windows.len());
    if lent.len() >= cap {
        let back = lent
            .iter()
            .min()
            .map_or(0, |t| t.saturating_sub(at).div_ceil(1000));
        return Err(format!(
            "{} of {cap} sandboxes are already lent; the next comes back in {back} s",
            lent.len()
        ));
    }
    let n = match want {
        Some(id) => {
            let n = windows_slot(f, id)?;
            windows_free(f, &f.windows[&n])?;
            n
        }
        None => *f
            .windows
            .iter()
            .find(|(_, s)| windows_free(f, s).is_ok())
            .map(|(n, _)| n)
            .ok_or("no sandbox can be lent now: none is running and free")?,
    };
    f.windows_claim_count += 1;
    let session = format!("windows-claim-{}", f.windows_claim_count);
    let slot = f.windows.get_mut(&n).unwrap();
    slot.windows.get_or_insert_with(Default::default).claim = Some(crate::windows::WindowsClaim {
        session: session.clone(),
        viewer: c.viewer.to_string(),
        client: c.client.to_string(),
        claimed_at: at,
        expires_at: at + cfg.windows_claim_secs * 1000,
        last_action_at: None,
    });
    Ok(format!("{session} on {}", slot.id))
}

/// Give a sandbox back: it is restored from the gold snapshot, so nothing the viewer did
/// survives them and the next viewer finds the idle desktop. The reset is owed until a launch
/// of its generation reports running (`observe_windows`), so a host lost or a coordinator
/// restarted before then, which drops the slot and its generation with it, still resets the
/// desktop when the host reports it again.
fn release_windows(f: &mut Fleet, n: u32, at: u64) {
    let Some(slot) = f.windows.get_mut(&n) else {
        return;
    };
    let Some(claim) = slot.windows.as_mut().and_then(|w| w.claim.take()) else {
        return;
    };
    bump(slot, &format!("return:{}", claim.session), at);
    if let (Some(host), Some(i)) = (slot.host.clone(), windows_index(slot)) {
        f.windows_to_reset.insert(
            crate::windows::key(&host, i),
            crate::windows::ResetOwed {
                session: claim.session.clone(),
                generation: Some(slot.generation),
            },
        );
    }
    f.last_windows_claim_end.insert(claim.client, at);
}

/// The take-over actions sandbox `n` offers: those its host's hello listed, and `reset`.
pub fn windows_offers(f: &Fleet, n: u32) -> Vec<crate::windows::WindowsAction> {
    let listed = f
        .windows
        .get(&n)
        .and_then(|s| s.host.as_ref())
        .and_then(|h| f.hosts.get(h))
        .map(|h| h.windows_actions.as_slice())
        .unwrap_or_default();
    crate::windows::WindowsAction::offered(listed)
}

fn windows_action(
    state: &mut State,
    c: &Ctx,
    claim: Option<&str>,
    action: crate::windows::WindowsAction,
) -> Result<String, String> {
    use crate::windows::WindowsAction;
    let at = c.at;
    let f = &mut state.fleet;
    let n = own_windows(f, c.viewer)
        .ok_or("sandbox actions need a sandbox of your own (claim_windows)")?;
    check_windows_claim(f, n, claim)?;
    // Only what the sandbox's host carries out: an agent skips the whole assignment line that
    // names an action it does not know.
    if !windows_offers(f, n).contains(&action) {
        return Err(format!(
            "{}'s host does not support {}",
            f.windows[&n].id,
            action.as_str()
        ));
    }
    let slot = f.windows.get_mut(&n).unwrap();
    let generation = slot.generation;
    let w = slot.windows.as_mut().unwrap();
    let k = w.claim.as_mut().unwrap();
    if let Some(last) = k.last_action_at {
        let next = last + crate::windows::ACTION_INTERVAL_MS;
        if at < next {
            return Err(format!("one action a second; wait {} ms", next - at));
        }
    }
    k.last_action_at = Some(at);
    let session = k.session.clone();
    if action == WindowsAction::Reset {
        // A reset restores the sandbox's VM, timed like any relaunch.
        bump(slot, &format!("reset:{session}"), at);
        return Ok(format!("reset {}", slot.id));
    }
    f.windows_action_seq += 1;
    w.actions.push(crate::windows::WindowsActionReq {
        seq: f.windows_action_seq,
        action,
        at,
        generation,
    });
    Ok(format!("{} on {}", action.as_str(), slot.id))
}

// ---------------------------------------------------------------------------
// Minecraft mode
// ---------------------------------------------------------------------------

fn minecraft_claim(s: &Slot) -> Option<&crate::minecraft::MinecraftClaim> {
    s.minecraft.as_ref().and_then(|m| m.claim.as_ref())
}

fn minecraft_index(s: &Slot) -> Option<u32> {
    s.minecraft.as_ref().map(|m| m.index)
}

/// Running the launch it is asked to run.
fn running_current(s: &Slot) -> bool {
    s.observed.phase == "running" && s.observed.generation >= s.generation
}

/// A server's new launch: its generation moves, timed from `at`, and any fork the old one
/// was is over.
fn bump_minecraft(slot: &mut Slot, by: &str, at: u64) {
    bump(slot, by, at);
    if let Some(m) = slot.minecraft.as_mut() {
        m.fork = None;
        m.forked_from = None;
    }
}

/// The servers of a host's hello: one per index below `count`, the host's others removed. A
/// server keeps its name and launch across a hello that keeps its index.
fn minecraft_hello(f: &mut Fleet, host: &str, count: u32, at: u64) {
    f.minecraft.retain(|_, s| {
        s.host.as_deref() != Some(host) || minecraft_index(s).is_some_and(|i| i < count)
    });
    for index in 0..count {
        let exists = f
            .minecraft
            .values()
            .any(|s| s.host.as_deref() == Some(host) && minecraft_index(s) == Some(index));
        if exists {
            continue;
        }
        let n = (1..).find(|n| !f.minecraft.contains_key(n)).unwrap();
        let mut s = new_slot(
            minecraft_name(n),
            Half::Fcvm,
            Some(host.to_string()),
            None,
            at,
        );
        s.minecraft = Some(crate::minecraft::MinecraftSlot {
            index,
            ..Default::default()
        });
        f.minecraft.insert(n, s);
    }
}

/// A server's report. Its launch's first frame is the time its bots got their first chunk, on
/// the host's clock, kept between the click and this read as a sandbox's desktop time is. A
/// fork's target that reaches `running` on the fork's generation ends the fork and becomes the
/// last fork.
fn observe_minecraft(f: &mut Fleet, n: u32, host: &str, r: &SlotReport, at: u64) {
    let Some(slot) = f.minecraft.get_mut(&n) else {
        return;
    };
    if slot.host.as_deref() != Some(host) {
        return;
    }
    if r.phase == "running" && slot.observed.phase != "running" {
        count_restore(&mut f.counters, at);
    }
    if r.generation > slot.generation {
        slot.generation = r.generation;
    }
    // The host came back running a world that was lent: reset it before anyone borrows it,
    // and keep owing the reset until the fresh launch reports running.
    if let Some(key) = minecraft_index(slot).map(|i| crate::minecraft::key(host, i)) {
        if let Some(owed) = f.minecraft_to_reset.get_mut(&key) {
            match owed.generation {
                Some(g) if r.phase == "running" && r.generation >= g => {
                    f.minecraft_to_reset.remove(&key);
                }
                Some(g) if slot.generation >= g => {}
                _ => {
                    slot.generation = slot
                        .generation
                        .max(owed.generation.unwrap_or(0).saturating_sub(1));
                    bump_minecraft(slot, &format!("return:{}", owed.session), at);
                    owed.generation = Some(slot.generation);
                }
            }
        }
    }
    let timed = slot
        .last_relaunch
        .as_ref()
        .is_some_and(|rl| rl.first_frame_at.is_some());
    let clicked = slot.last_relaunch.is_some();
    note_report(slot, r, at);
    let running = note_running(slot, r, at);
    if running && !timed {
        if let (Some(rl), Some(chunk)) = (slot.last_relaunch.as_mut(), r.first_chunk_at) {
            if rl.generation == r.generation && rl.first_frame_at == Some(at) {
                let chunk = chunk.min(at);
                if clicked {
                    rl.first_frame_at = Some(chunk.max(rl.clicked_at.min(at)));
                } else {
                    // Nobody clicked (a launch at the host's start): the launch began
                    // `total_ms` before its first chunk, both on the host's clock.
                    let took = r.timings.get("total_ms").copied().unwrap_or(0);
                    rl.clicked_at = chunk.saturating_sub(took);
                    rl.first_frame_at = Some(chunk);
                }
            }
        }
    }
    let generation = slot.generation;
    let Some(m) = slot.minecraft.as_mut() else {
        return;
    };
    let Some(k) = m.fork.clone() else {
        return;
    };
    if k.generation < generation {
        m.fork = None;
        return;
    }
    if !(running && r.generation >= k.generation) {
        return;
    }
    m.fork = None;
    let first_chunk_at = slot
        .last_relaunch
        .as_ref()
        .filter(|rl| rl.generation == k.generation)
        .and_then(|rl| rl.first_frame_at);
    let time = |k: &str| r.timings.get(k).copied();
    f.last_minecraft_fork = Some(crate::minecraft::LastFork {
        from: k.slot,
        to: slot.id.clone(),
        clicked_at: k.clicked_at,
        by: k.by,
        first_chunk_at,
        snapshot_ms: time("snapshot_ms"),
        restore_ms: time("restore_ms"),
        total_ms: time("total_ms"),
    });
}

/// End every borrow whose time is up at `at`. A tick does it, and so does every Minecraft
/// intent before it is judged, so a borrow ends at its deadline and not at the next tick.
fn expire_minecraft(f: &mut Fleet, at: u64) {
    let expired: Vec<u32> = f
        .minecraft
        .iter()
        .filter(|(_, s)| minecraft_claim(s).is_some_and(|c| at >= c.expires_at))
        .map(|(n, _)| *n)
        .collect();
    for n in expired {
        release_minecraft(f, n, at);
    }
}

/// Remove the servers `gone` picks. A lent one's borrow ends here (its client's cooldown
/// starts), and it is marked to be reset when its host reports it again, as
/// [`forget_windows`] does for a sandbox.
pub fn forget_minecraft(f: &mut Fleet, gone: impl Fn(&Slot) -> bool, at: u64) {
    for s in f.minecraft.values().filter(|s| gone(s)) {
        if let (Some(c), Some(host), Some(i)) = (minecraft_claim(s), &s.host, minecraft_index(s)) {
            f.last_minecraft_claim_end.insert(c.client.clone(), at);
            f.minecraft_to_reset.insert(
                crate::minecraft::key(host, i),
                crate::minecraft::ResetOwed {
                    session: c.session.clone(),
                    generation: None,
                },
            );
        }
    }
    f.minecraft.retain(|_, s| !gone(s));
}

fn own_minecraft(f: &Fleet, viewer: &str) -> Option<u32> {
    f.minecraft
        .iter()
        .find(|(_, s)| minecraft_claim(s).is_some_and(|c| c.viewer == viewer))
        .map(|(n, _)| *n)
}

fn check_minecraft_claim(f: &Fleet, n: u32, claim: Option<&str>) -> Result<(), String> {
    let session = &minecraft_claim(&f.minecraft[&n]).unwrap().session;
    match claim {
        Some(k) if k != session => Err(format!("{k:?} is not this viewer's server")),
        _ => Ok(()),
    }
}

/// The server named `id`.
fn minecraft_slot(f: &Fleet, id: &str) -> Result<u32, String> {
    minecraft_number(id)
        .filter(|n| f.minecraft.contains_key(n))
        .ok_or_else(|| format!("no server {id:?}"))
}

/// Servers a viewer may borrow: on a host that is not draining, running their current
/// launch, not lent.
fn minecraft_free(f: &Fleet, s: &Slot) -> Result<(), String> {
    if minecraft_claim(s).is_some() {
        return Err(format!("{} is lent to another viewer", s.id));
    }
    if host_draining(&f.hosts, s) {
        return Err(format!("{} is leaving the fleet", s.id));
    }
    if !running_current(s) {
        return Err(format!("{} is not running", s.id));
    }
    Ok(())
}

/// Restore one server, or every one, from the gold snapshot: its generation moves and the
/// launch is timed from this click.
fn minecraft_reset(state: &mut State, c: &Ctx, slot: Option<&str>) -> Result<String, String> {
    let at = c.at;
    if state.fleet.power.state == "asleep" {
        return Err("the hosts are asleep".into());
    }
    let f = &mut state.fleet;
    match slot {
        Some(id) => {
            let n = minecraft_slot(f, id)?;
            rate(
                &f.last_minecraft_reset_at,
                c.client,
                at,
                state.config.relaunch_interval_ms,
                "reset",
            )?;
            let hosts = f.hosts.clone();
            let s = f.minecraft.get_mut(&n).unwrap();
            if minecraft_claim(s).is_some() {
                return Err(format!("{} is lent to a viewer", s.id));
            }
            if host_draining(&hosts, s) {
                return Err(format!("{} is on a draining host", s.id));
            }
            bump_minecraft(s, &format!("viewer:{}", c.viewer), at);
            f.last_minecraft_reset_at.insert(c.client.to_string(), at);
            Ok(format!("reset {}", s.id))
        }
        None => {
            rate(
                &f.last_minecraft_reset_all_at,
                c.client,
                at,
                crate::minecraft::RESET_ALL_INTERVAL_MS,
                "reset all",
            )?;
            let hosts = f.hosts.clone();
            let mut n = 0;
            for s in f.minecraft.values_mut() {
                if minecraft_claim(s).is_some() || host_draining(&hosts, s) {
                    continue;
                }
                bump_minecraft(s, "reset_all", at);
                n += 1;
            }
            if n == 0 {
                return Err("no server can be reset now".into());
            }
            f.last_minecraft_reset_all_at
                .insert(c.client.to_string(), at);
            f.last_minecraft_reset_all = Some(crate::minecraft::ResetAll {
                clicked_at: at,
                by: format!("viewer:{}", c.viewer),
                servers: n,
            });
            Ok(format!("reset all {n} servers"))
        }
    }
}

/// Copy a running server's world into the next server on its host (in index order, wrapping)
/// that runs its current launch, is not lent and is not on a draining host. The target's
/// generation moves with the fork set; the source is not touched.
fn minecraft_fork(state: &mut State, c: &Ctx, id: &str) -> Result<String, String> {
    let at = c.at;
    if state.fleet.power.state == "asleep" {
        return Err("the hosts are asleep".into());
    }
    let f = &mut state.fleet;
    let n = minecraft_slot(f, id)?;
    rate(
        &f.last_minecraft_fork_at,
        c.client,
        at,
        crate::minecraft::FORK_INTERVAL_MS,
        "fork",
    )?;
    let source = &f.minecraft[&n];
    if !running_current(source) {
        return Err(format!("{} is not running", source.id));
    }
    let from = source.id.clone();
    let index = minecraft_index(source).unwrap_or_default();
    let mut peers: Vec<(u32, u32)> = f
        .minecraft
        .iter()
        .filter(|(_, s)| s.host.is_some() && s.host == source.host)
        .filter_map(|(k, s)| Some((minecraft_index(s)?, *k)))
        .collect();
    peers.sort();
    let at_source = peers.iter().position(|(_, k)| *k == n).unwrap_or_default();
    let target = (1..peers.len())
        .map(|d| peers[(at_source + d) % peers.len()].1)
        .find(|k| {
            let s = &f.minecraft[k];
            minecraft_claim(s).is_none() && running_current(s) && !host_draining(&f.hosts, s)
        })
        .ok_or_else(|| format!("no server on {from}'s host can take a copy"))?;
    let t = f.minecraft.get_mut(&target).unwrap();
    bump_minecraft(t, &format!("fork:{from}"), at);
    let generation = t.generation;
    let m = t.minecraft.get_or_insert_with(Default::default);
    m.fork = Some(crate::minecraft::PendingFork {
        generation,
        slot: from.clone(),
        index,
        clicked_at: at,
        by: format!("viewer:{}", c.viewer),
    });
    m.forked_from = Some(crate::minecraft::ForkedFrom {
        generation,
        slot: from.clone(),
    });
    let to = t.id.clone();
    f.last_minecraft_fork_at.insert(c.client.to_string(), at);
    Ok(format!("fork {from} into {to}"))
}

fn claim_minecraft(state: &mut State, c: &Ctx, want: Option<&str>) -> Result<String, String> {
    let at = c.at;
    let cfg = &state.config;
    let f = &mut state.fleet;
    if f.minecraft
        .values()
        .any(|s| minecraft_claim(s).is_some_and(|k| k.client == c.client))
    {
        return Err("one server per client at a time".into());
    }
    if let Some(end) = f.last_minecraft_claim_end.get(c.client) {
        let free_at = end + cfg.minecraft_claim_cooldown_secs * 1000;
        if at < free_at {
            return Err(format!(
                "your last server was returned {} s ago; borrow again in {} s",
                (at - end) / 1000,
                (free_at - at).div_ceil(1000)
            ));
        }
    }
    let lent: Vec<u64> = f
        .minecraft
        .values()
        .filter_map(minecraft_claim)
        .map(|k| k.expires_at)
        .collect();
    let cap = cfg.minecraft_lend_cap(f.minecraft.len());
    if lent.len() >= cap {
        let back = lent
            .iter()
            .min()
            .map_or(0, |t| t.saturating_sub(at).div_ceil(1000));
        return Err(format!(
            "{} of {cap} servers are already lent; the next comes back in {back} s",
            lent.len()
        ));
    }
    let n = match want {
        Some(id) => {
            let n = minecraft_slot(f, id)?;
            minecraft_free(f, &f.minecraft[&n])?;
            n
        }
        None => *f
            .minecraft
            .iter()
            .find(|(_, s)| minecraft_free(f, s).is_ok())
            .map(|(n, _)| n)
            .ok_or("no server can be lent now: none is running and free")?,
    };
    f.minecraft_claim_count += 1;
    let session = format!("minecraft-claim-{}", f.minecraft_claim_count);
    let slot = f.minecraft.get_mut(&n).unwrap();
    slot.minecraft.get_or_insert_with(Default::default).claim =
        Some(crate::minecraft::MinecraftClaim {
            session: session.clone(),
            viewer: c.viewer.to_string(),
            client: c.client.to_string(),
            claimed_at: at,
            expires_at: at + cfg.minecraft_claim_secs * 1000,
        });
    Ok(format!("{session} on {}", slot.id))
}

/// Give a server back: it is restored from the gold snapshot, so nothing the viewer built
/// survives them. The reset is owed until a launch of its generation reports running
/// (`observe_minecraft`), as `release_windows` owes a sandbox's.
fn release_minecraft(f: &mut Fleet, n: u32, at: u64) {
    let Some(slot) = f.minecraft.get_mut(&n) else {
        return;
    };
    let Some(claim) = slot.minecraft.as_mut().and_then(|m| m.claim.take()) else {
        return;
    };
    bump_minecraft(slot, &format!("return:{}", claim.session), at);
    if let (Some(host), Some(i)) = (slot.host.clone(), minecraft_index(slot)) {
        f.minecraft_to_reset.insert(
            crate::minecraft::key(&host, i),
            crate::minecraft::ResetOwed {
                session: claim.session.clone(),
                generation: Some(slot.generation),
            },
        );
    }
    f.last_minecraft_claim_end.insert(claim.client, at);
}
