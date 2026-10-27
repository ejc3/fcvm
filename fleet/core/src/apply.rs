//! `apply(state, event) -> state`: the only way the fleet state changes. No clock, no I/O,
//! no randomness; the time is in the event (and a claim's token arrives as the intent's
//! `secret`).

use crate::director::viewpoints;
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
        Event::ViewerJoined { viewer, client, .. } => {
            let key = client_key(viewer, client.as_deref()).to_string();
            state.fleet.viewers.insert(viewer.clone(), key);
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
            None
        }
        Event::Tick { .. } => {
            tick(state, at);
            budget_round(&mut state.fleet, at);
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
                } => {
                    // The connection names the host; a hello naming another is ignored.
                    if named == host {
                        hello(&mut state.fleet, host, arch, gpus, *capacity, at);
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
            }
            None
        }
        Event::AgentGone { host, .. } => {
            let f = &mut state.fleet;
            f.hosts.remove(host);
            f.slots.retain(|_, s| s.host.as_deref() != Some(host));
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

/// Sleep after `idle_scale_down_secs` with no active viewer (never during a claim or a
/// race), wake on an active viewer, and be awake once the image is complete.
fn power_step(state: &mut State, at: u64) {
    if !state.config.power_managed {
        return;
    }
    let idle_ms = state.config.idle_scale_down_secs * 1000;
    let interval = state.config.race_interval_secs;
    let f = &mut state.fleet;
    let active = f.viewers.keys().filter(|v| !f.hidden.contains(*v)).count();
    if active > 0 {
        f.power.idle_since = None;
    } else {
        f.power.idle_since.get_or_insert(at);
    }
    let claims = f.slots.values().any(|s| s.claim.is_some());
    let racing = race_running(f);
    if f.power.state == "asleep" {
        if active > 0 {
            f.power.state = "waking".into();
            f.power.woke_at = Some(at);
            f.power.asleep_since = None;
            f.power.hosts.clear();
        }
        return;
    }
    let idle = f.power.idle_since.is_some_and(|t| at >= t + idle_ms);
    if active == 0 && idle && !claims && !racing {
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
                Some(k) if k.verdict == "error" => {
                    return Err(format!("the text check failed: {}", k.reason))
                }
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
            while f.scene.words.len() > MAX_WORDS {
                f.scene.words.remove(0);
            }
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
        Intent::ClaimGpu => claim(state, c),
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
            let mut cam = crate::scene::Scene {
                epoch_unix_ms: 0,
                camera: claim.camera.clone(),
                time_of_day: 0.0,
                words: vec![],
                lights: vec![],
            };
            let to = Keyframe {
                intent: Some(c.id.to_string()),
                ..drag(&cam.camera_at(now), *yaw_deg, *pitch_deg, *dive)
            };
            cam.steer(now, to);
            claim.camera = cam.camera;
            Ok("claim camera".into())
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
    let vp = &viewpoints()[f.director.current];
    let now = f.scene.t_at(at);
    let start = Keyframe {
        t: now,
        pos: vp.pos,
        look: vp.look,
        fov_deg: vp.fov_deg,
        intent: Some(c.id.to_string()),
    };
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
            while f.scene.words.len() > MAX_WORDS {
                f.scene.words.remove(0);
            }
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
    if let Some(tod) = f.director.tick(now_t) {
        f.scene.time_of_day = tod;
    }
    let cam = f.director.camera();
    if f.scene.camera != cam {
        f.scene.camera = cam;
    }
    history_step(state, at);
    let f = &mut state.fleet;
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
    }
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
    if r.phase != "running" {
        slot.observed.fps = None;
        slot.observed.gpu_stats = None;
        slot.observed.budget_fit = None;
        slot.observed.render_ms_max = None;
        slot.observed.frame_t = None;
        return;
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
    if let Some(c) = slot.claim.as_mut() {
        if c.generation == r.generation && c.first_frame_at.is_none() {
            c.first_frame_at = Some(at);
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
    let overloaded = tiles.iter().any(|s| {
        s.observed
            .render_ms_max
            .is_some_and(|ms| ms > crate::budget::OVERLOAD_MS)
    });
    let newest = tiles
        .iter()
        .filter_map(|s| s.observed.frame_t)
        .fold(0.0, f64::max);
    f.budget.round(&fits, overloaded, newest);
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
/// - fcvm places: `max(min(fcvm_tiles, max_tiles), fcvm GPUs)`. The GPUs are grouped into
///   one band per host and the places left over are one dark band below them.
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
    // the fcvm side has `fcvm_tiles` places, or one per live GPU when there are more.
    let ec2_places: Vec<u32> = f.ec2.keys().copied().take(c.max_tiles).collect();
    let fcvm_places = if asleep {
        0
    } else {
        c.fcvm_tiles.min(c.max_tiles).max(active.len())
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
