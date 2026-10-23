//! `apply(state, event) -> state`: the only way the fleet state changes. No clock, no I/O,
//! no randomness; the time is in the event (and a claim's token arrives as the intent's
//! `secret`).

use crate::director::viewpoints;
use crate::event::{Event, Intent};
use crate::protocol::{AgentMsg, HealthReport, SlotReport};
use crate::scene::{drag, Keyframe, Light, Word, MAX_LIGHTS, MAX_WORDS};
use crate::state::*;
use crate::tiling::{bands, Half, Rect};
use std::collections::BTreeMap;

/// How long a reclaimed host stays in the failure list after it is gone.
pub const RECLAIMED_KEEP_MS: u64 = 60_000;
/// Races counted in the scoreboard's medians.
pub const SCOREBOARD_RACES: usize = 20;

/// Apply one event. The version moves exactly when the state changed; an intent id seen
/// before changes nothing (its first receipt stays in `state.receipts`).
pub fn apply(mut state: State, event: &Event) -> State {
    let before = state.fleet.clone();
    let receipt = apply_inner(&mut state, event);
    power_step(&mut state, event.at());
    retile(&mut state.fleet, &state.config, event.at());
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

/// The words in the sky now, which a new word must be checked together with.
pub fn sky(state: &State) -> Vec<String> {
    state
        .fleet
        .scene
        .words
        .iter()
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
                Err(reason) => (Outcome::Refused, Some(reason.clone()), reason),
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
                check: check.clone(),
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
        Event::ScaleInstances { instances, .. } => {
            let p = &mut state.fleet.power;
            if p.state == "waking" {
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
                } => {
                    // The connection names the host; a hello naming another is ignored.
                    if named == host {
                        hello(&mut state.fleet, host, arch, gpus, *capacity, at);
                        let p = &mut state.fleet.power;
                        if p.state == "waking" {
                            let w = p.hosts.entry(host.clone()).or_default();
                            w.agent_at.get_or_insert(at);
                            // The host's own clock: kept only when it falls inside this wake.
                            if let Some(t) = store_ready_at
                                .filter(|t| p.woke_at.is_some_and(|w| *t >= w) && *t <= at)
                            {
                                w.store_ready_at.get_or_insert(t);
                            }
                        }
                    }
                }
                AgentMsg::Slot(r) => {
                    if let Some(n) = slot_number(&r.id) {
                        observe_slot(&mut state.fleet, Half::Fcvm, n, Some(host), r, at);
                    }
                }
                AgentMsg::Health(h) => health(&mut state.fleet, host, h),
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
                AgentMsg::Aggregate { frame_url } => {
                    if let Some(h) = state.fleet.hosts.get_mut(host) {
                        h.aggregate_url = Some(frame_url.clone());
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
                AgentMsg::Quarantine { sbdf, error, .. } => {
                    quarantine(&mut state.fleet, host, sbdf, error, at);
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
            state.fleet.quarantined.remove(&gpu_key(host, sbdf));
            None
        }
    }
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
        s.position.is_some()
            && s.claim.is_none()
            && (s.observed.generation < s.generation
                || matches!(
                    s.observed.phase.as_str(),
                    "pending" | "launching" | "stopping" | "restoring" | "starting"
                ))
    };
    !race_running(f) && !f.slots.values().chain(f.ec2.values()).any(starting)
}

/// Every placed tile of both halves runs its current generation.
fn image_complete(f: &Fleet) -> bool {
    let done = |s: &Slot| {
        s.position.is_none()
            || (s.observed.phase == "running" && s.observed.generation >= s.generation)
    };
    f.n > 0 && f.slots.values().all(done) && f.ec2.values().all(done)
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
        f.power.gpus_before_sleep = f.slots.values().filter(|s| s.position.is_some()).count();
        f.power.state = "asleep".into();
        f.power.asleep_since = Some(at);
        f.power.woke_at = None;
        f.power.hosts.clear();
        f.next_race_at = None;
        return;
    }
    if f.power.state == "waking" {
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
        if image_complete(f) {
            let started = f.power.woke_at.unwrap_or(at);
            let gpus = f
                .slots
                .values()
                .chain(f.ec2.values())
                .filter(|s| s.position.is_some())
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
fn quarantine(f: &mut Fleet, host: &str, sbdf: &str, error: &str, at: u64) {
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
            state.fleet.director.vote(c.client, target, c.id)?;
            Ok(format!("nudge {target}"))
        }
        Intent::Word { text } => {
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
            });
            while f.scene.words.len() > MAX_WORDS {
                f.scene.words.remove(0);
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
    for slot in f.ec2.values_mut() {
        bump(slot, by, at);
        if let Some(k) = slot.position {
            race.ec2.tiles.insert(
                slot.id.clone(),
                RaceTile {
                    tile: format!("ec2:{}", k + 1),
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
        observed: Observed {
            phase: "pending".into(),
            ..Default::default()
        },
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
    slot.observed.phase = r.phase.clone();
    slot.observed.generation = r.generation;
    slot.observed.vm = r.vm.clone();
    slot.observed.frame_url = r.frame_url.clone();
    slot.observed.timings = r.timings.clone();
    slot.observed.error = r.error.clone();
    slot.observed.retry_at = r.retry_in_ms.map(|ms| at + ms);
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
        return;
    }
    if let Some(rl) = slot.last_relaunch.as_mut() {
        if rl.generation == r.generation && rl.first_frame_at.is_none() {
            rl.first_frame_at = Some(at);
            rl.phases = r.timings.clone();
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

fn health(f: &mut Fleet, host: &str, h: &HealthReport) {
    let Some(n) = slot_number(&h.id) else { return };
    let Some(slot) = f.slots.get_mut(&n) else {
        return;
    };
    if slot.host.as_deref() != Some(host) || slot.observed.phase != "running" {
        return;
    }
    slot.observed.fps = h.fps.is_finite().then_some(h.fps);
    slot.observed.gpu_stats = Some(h.gpu.clone());
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

/// Recompute N, the EC2 tiles wanted and every slot's rectangle. Deterministic in the
/// state, so it runs after every event.
///
/// - fcvm GPUs: slots whose host is present and not draining, in slot-number order; a
///   GPU claimed from standby is left out, a borrowed wall tile keeps its place.
/// - EC2 tiles wanted: `min(ec2_max, max_tiles, fcvm GPUs)`; `ec2-1..ec2-D` exist, higher
///   ones are dropped (the driver terminates their instances).
/// - EC2 instances: EC2 tiles that report an instance.
/// - N = min(max_tiles, max(fcvm GPUs, EC2 instances)); the first N fcvm slots are grouped
///   into one band per host, the EC2 half repeats the layout, and the rest of each half is
///   dark. fcvm slots beyond N are standby.
fn retile(f: &mut Fleet, c: &Config, at: u64) {
    let active: Vec<u32> = f
        .slots
        .iter()
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
    // Asleep, the EC2 half has no tiles; waking, it launches as many as the fcvm half had.
    let fcvm_gpus = match f.power.state.as_str() {
        "asleep" => 0,
        "waking" => active.len().max(f.power.gpus_before_sleep),
        _ => active.len(),
    };
    let want_ec2 = c.ec2_max.min(c.max_tiles).min(fcvm_gpus) as u32;
    f.ec2.retain(|n, _| *n <= want_ec2);
    for n in 1..=want_ec2 {
        f.ec2
            .entry(n)
            .or_insert_with(|| new_slot(ec2_name(n), Half::Ec2, None, None, at));
    }
    let instances = f.ec2.values().filter(|s| s.observed.vm.is_some()).count();
    let n = c.max_tiles.min(active.len().max(instances));
    f.n = n;
    // The placed slots (the first n by number) grouped by host, hosts ordered by their
    // lowest placed slot; one band per host, then one band of dark tiles if EC2 has more
    // instances than the fcvm half has GPUs.
    let mut groups: Vec<(String, Vec<u32>)> = Vec::new();
    for num in active.iter().take(n) {
        let host = f.slots[num].host.clone().expect("active slots have a host");
        match groups.iter_mut().find(|(h, _)| *h == host) {
            Some((_, v)) => v.push(*num),
            None => groups.push((host, vec![*num])),
        }
    }
    let placed: usize = groups.iter().map(|(_, v)| v.len()).sum();
    let mut counts: Vec<usize> = groups.iter().map(|(_, v)| v.len()).collect();
    if n > placed {
        counts.push(n - placed);
    }
    let bands = if n == 0 {
        Vec::new()
    } else {
        bands(Half::Fcvm, &counts, c.image_aspect, c.tile_aspect)
    };
    f.layout = bands
        .iter()
        .flat_map(|(_, tiles)| tiles.iter().copied())
        .collect();
    for s in f.slots.values_mut() {
        s.position = None;
        s.rect = None;
    }
    for h in f.hosts.values_mut() {
        h.region = None;
    }
    let mut k = 0;
    for (gi, (host, nums)) in groups.iter().enumerate() {
        if let Some(h) = f.hosts.get_mut(host) {
            h.region = Some(bands[gi].0);
        }
        for num in nums {
            let s = f.slots.get_mut(num).unwrap();
            s.position = Some(k);
            s.rect = Some(f.layout[k]);
            k += 1;
        }
    }
    for (num, s) in f.ec2.iter_mut() {
        let k = (*num - 1) as usize;
        s.position = (k < n).then_some(k);
        s.rect = f.layout.get(k).map(|r| Rect { x: r.x + 0.5, ..*r });
    }
    drop_missing_from_races(f, at);
}
