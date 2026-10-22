//! `apply(state, event) -> state`: the only way the fleet state changes. No clock, no I/O,
//! no randomness; the time is in the event.

use crate::event::{CameraMove, Event, Intent};
use crate::protocol::{AgentMsg, HealthReport, SlotReport};
use crate::scene::{drag, Light, Word, MAX_LIGHTS, MAX_WORDS};
use crate::state::*;
use crate::tiling::{half_rects, Half};

/// Apply one event. The version moves exactly when the state changed; an intent id seen
/// before changes nothing (its first receipt stays in `state.receipts`).
pub fn apply(mut state: State, event: &Event) -> State {
    let before = state.fleet.clone();
    let receipt = apply_inner(&mut state, event);
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

/// The checks `apply` would refuse a `word` on before its text check, so the coordinator
/// spends no text-check call on an intent that is refused anyway. Returns the refusal
/// reason, or `None` when the intent should go on to the text check.
pub fn precheck_word(
    state: &State,
    viewer: &str,
    args: &serde_json::Value,
    at: u64,
) -> Option<String> {
    match Intent::parse("word", args) {
        Err(reason) => Some(reason),
        Ok(_) => word_rate(state, viewer, at).err(),
    }
}

fn word_rate(state: &State, viewer: &str, at: u64) -> Result<(), String> {
    match state.fleet.last_word_at.get(viewer) {
        Some(&last) if at < last + state.config.word_interval_ms => Err(format!(
            "one word per {} s per viewer",
            state.config.word_interval_ms / 1000
        )),
        _ => Ok(()),
    }
}

fn apply_inner(state: &mut State, event: &Event) -> Option<Receipt> {
    let at = event.at();
    match event {
        Event::Intent {
            viewer,
            id,
            name,
            args,
            check,
            ..
        } => {
            if state.receipts.contains_key(id) {
                return None;
            }
            let result = Intent::parse(name, args)
                .and_then(|intent| intent_effect(state, viewer, &intent, check.as_ref(), at));
            let (outcome, reason, summary) = match result {
                Ok(summary) => (Outcome::Committed, None, summary),
                Err(reason) => (Outcome::Refused, Some(reason.clone()), reason),
            };
            // A refused word's text is never shown: the summary is the reason only.
            push_action(
                &mut state.fleet,
                Action {
                    at,
                    by: viewer.clone(),
                    name: name.clone(),
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
        Event::ViewerJoined { viewer, .. } => {
            state.fleet.viewers.insert(viewer.clone());
            None
        }
        Event::ViewerLeft { viewer, .. } => {
            let f = &mut state.fleet;
            f.viewers.remove(viewer);
            f.last_word_at.remove(viewer);
            f.last_light_at.remove(viewer);
            f.last_relaunch_at.remove(viewer);
            f.last_relaunch_all_at.remove(viewer);
            None
        }
        Event::Tick { .. } => {
            auto_relaunch(&mut state.fleet, at);
            None
        }
        Event::Agent { host, msg, .. } => {
            match msg {
                AgentMsg::Hello {
                    host: named,
                    arch,
                    gpus,
                    capacity,
                } => {
                    // The connection names the host; a hello naming another is ignored.
                    if named == host {
                        hello(&mut state.fleet, host, arch, gpus, *capacity, at);
                    }
                }
                AgentMsg::Slot(r) => {
                    if let Some(n) = slot_number(&r.id) {
                        observe_slot(&mut state.fleet, Half::Fcvm, n, Some(host), r, at);
                    }
                }
                AgentMsg::Health(h) => health(&mut state.fleet, host, h, at),
                AgentMsg::Draining { .. } => {
                    if let Some(h) = state.fleet.hosts.get_mut(host) {
                        h.draining = true;
                    }
                    drop_pending_of_host(&mut state.fleet, host, at);
                }
            }
            None
        }
        Event::AgentGone { host, .. } => {
            let f = &mut state.fleet;
            f.hosts.remove(host);
            f.slots.retain(|_, s| s.host.as_deref() != Some(host));
            drop_missing_pending(f, at);
            None
        }
        Event::Ec2 { report, .. } => {
            if let Some(n) = ec2_number(&report.id) {
                observe_slot(&mut state.fleet, Half::Ec2, n, None, report, at);
            }
            None
        }
    }
}

fn push_action(f: &mut Fleet, a: Action) {
    f.actions.push_back(a);
    while f.actions.len() > MAX_ACTIONS {
        f.actions.pop_front();
    }
}

fn intent_effect(
    state: &mut State,
    viewer: &str,
    intent: &Intent,
    check: Option<&TextCheck>,
    at: u64,
) -> Result<String, String> {
    let by = format!("viewer:{viewer}");
    match intent {
        Intent::Relaunch { tile } => {
            let (half, n) = state
                .find_slot(tile)
                .ok_or_else(|| format!("no GPU behind {tile}"))?;
            rate(
                &state.fleet.last_relaunch_at,
                viewer,
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
            bump(slot, &by, at);
            let summary = format!("relaunch {}", slot.id);
            f.last_relaunch_at.insert(viewer.to_string(), at);
            Ok(summary)
        }
        Intent::RelaunchAll => {
            rate(
                &state.fleet.last_relaunch_all_at,
                viewer,
                at,
                state.config.relaunch_all_interval_ms,
                "relaunch_all",
            )?;
            let f = &mut state.fleet;
            f.last_relaunch_all_at.insert(viewer.to_string(), at);
            let mut fcvm_pending = std::collections::BTreeMap::new();
            let mut ec2_pending = std::collections::BTreeMap::new();
            let hosts = f.hosts.clone();
            for slot in f.slots.values_mut() {
                if host_draining(&hosts, slot) {
                    continue;
                }
                bump(slot, "relaunch_all", at);
                if slot.position.is_some() {
                    fcvm_pending.insert(slot.id.clone(), slot.generation);
                }
            }
            for slot in f.ec2.values_mut() {
                bump(slot, "relaunch_all", at);
                if slot.position.is_some() {
                    ec2_pending.insert(slot.id.clone(), slot.generation);
                }
            }
            let count = fcvm_pending.len() + ec2_pending.len();
            f.relaunch_all = Some(RelaunchAll {
                started_at: at,
                by,
                fcvm_done_at: None,
                ec2_done_at: None,
                fcvm_last: None,
                ec2_last: None,
                fcvm_pending,
                ec2_pending,
            });
            Ok(format!("relaunch all ({count} tiles)"))
        }
        Intent::SetAutoRelaunch { secs } => {
            let c = &state.config;
            if let Some(s) = secs {
                if *s < c.auto_relaunch_min_secs || *s > c.auto_relaunch_max_secs {
                    return Err(format!(
                        "auto relaunch must be off or {} to {} s",
                        c.auto_relaunch_min_secs, c.auto_relaunch_max_secs
                    ));
                }
            }
            state.fleet.settings.auto_relaunch_secs = *secs;
            Ok(match secs {
                Some(s) => format!("auto relaunch every {s} s"),
                None => "auto relaunch off".into(),
            })
        }
        Intent::Camera(mv) => {
            let scene = &mut state.fleet.scene;
            let now = scene.t_at(at);
            match mv {
                CameraMove::Keyframes(k) => {
                    scene.camera = k.clone();
                    scene.trim_history(now);
                    Ok("camera path".into())
                }
                CameraMove::Drag {
                    yaw_deg,
                    pitch_deg,
                    dive,
                } => {
                    let to = drag(scene.camera_at(now), *yaw_deg, *pitch_deg, *dive);
                    scene.steer(now, to);
                    Ok("camera".into())
                }
            }
        }
        Intent::TimeOfDay { v } => {
            state.fleet.scene.time_of_day = *v;
            Ok(format!("time of day {v:.3}"))
        }
        Intent::Word { text } => {
            word_rate(state, viewer, at)?;
            match check {
                None => return Err("the text check did not run".into()),
                Some(c) if c.verdict == "error" => {
                    return Err(format!("the text check failed: {}", c.reason))
                }
                Some(c) if !c.allowed() => {
                    return Err(format!("the text check refused it: {}", c.reason))
                }
                Some(_) => {}
            }
            let f = &mut state.fleet;
            f.last_word_at.insert(viewer.to_string(), at);
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
            let f = &mut state.fleet;
            if let Some(&last) = f.last_light_at.get(viewer) {
                if at < last + state.config.light_interval_ms {
                    return Err("too many lights; wait a moment".into());
                }
            }
            f.last_light_at.insert(viewer.to_string(), at);
            let t = f.scene.t_at(at);
            f.scene.lights.push(Light {
                pos: *pos,
                color: *color,
                t,
            });
            while f.scene.lights.len() > MAX_LIGHTS {
                f.scene.lights.remove(0);
            }
            Ok("light".into())
        }
    }
}

/// Refuse when `viewer`'s last committed intent of this kind is less than `interval_ms` old.
fn rate(
    last: &std::collections::BTreeMap<String, u64>,
    viewer: &str,
    at: u64,
    interval_ms: u64,
    what: &str,
) -> Result<(), String> {
    match last.get(viewer) {
        Some(&t) if at < t + interval_ms => Err(format!(
            "one {what} per {} s per viewer",
            interval_ms.div_ceil(1000)
        )),
        _ => Ok(()),
    }
}

fn host_draining(hosts: &std::collections::BTreeMap<String, Host>, slot: &Slot) -> bool {
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

fn auto_relaunch(f: &mut Fleet, at: u64) {
    let Some(secs) = f.settings.auto_relaunch_secs else {
        return;
    };
    let due = |s: &Slot| {
        let base = s
            .last_relaunch
            .as_ref()
            .map_or(s.created_at, |r| r.clicked_at.max(s.created_at));
        at >= base + secs * 1000
    };
    let hosts = f.hosts.clone();
    for slot in f.slots.values_mut() {
        if !host_draining(&hosts, slot) && due(slot) {
            bump(slot, "auto", at);
        }
    }
    for slot in f.ec2.values_mut() {
        if due(slot) {
            bump(slot, "auto", at);
        }
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
        },
    );
    drop_missing_pending(f, at);
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
    }
}

fn observe_slot(f: &mut Fleet, half: Half, n: u32, host: Option<&str>, r: &SlotReport, at: u64) {
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
    let id = slot.id.clone();
    if let Some(ra) = f.relaunch_all.as_mut() {
        let (pending, done, last) = match half {
            Half::Fcvm => (
                &mut ra.fcvm_pending,
                &mut ra.fcvm_done_at,
                &mut ra.fcvm_last,
            ),
            Half::Ec2 => (&mut ra.ec2_pending, &mut ra.ec2_done_at, &mut ra.ec2_last),
        };
        if pending.get(&id).is_some_and(|g| r.generation >= *g) {
            pending.remove(&id);
            if pending.is_empty() && done.is_none() {
                *done = Some(at);
                *last = Some(id);
            }
        }
    }
}

fn health(f: &mut Fleet, host: &str, h: &HealthReport, _at: u64) {
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

/// A relaunch-all no longer waits for slots that left or whose host drains; if that
/// empties a half, the half is done now.
fn drop_pending_of_host(f: &mut Fleet, host: &str, at: u64) {
    let gone: Vec<String> = f
        .slots
        .values()
        .filter(|s| s.host.as_deref() == Some(host))
        .map(|s| s.id.clone())
        .collect();
    if let Some(ra) = f.relaunch_all.as_mut() {
        let before = ra.fcvm_pending.len();
        for id in &gone {
            ra.fcvm_pending.remove(id);
        }
        if before > 0 && ra.fcvm_pending.is_empty() && ra.fcvm_done_at.is_none() {
            ra.fcvm_done_at = Some(at);
        }
    }
}

fn drop_missing_pending(f: &mut Fleet, at: u64) {
    let slots = &f.slots;
    let ec2 = &f.ec2;
    if let Some(ra) = f.relaunch_all.as_mut() {
        let before = ra.fcvm_pending.len();
        ra.fcvm_pending
            .retain(|id, _| slot_number(id).is_some_and(|n| slots.contains_key(&n)));
        if before > 0 && ra.fcvm_pending.is_empty() && ra.fcvm_done_at.is_none() {
            ra.fcvm_done_at = Some(at);
        }
        let before = ra.ec2_pending.len();
        ra.ec2_pending
            .retain(|id, _| ec2_number(id).is_some_and(|n| ec2.contains_key(&n)));
        if before > 0 && ra.ec2_pending.is_empty() && ra.ec2_done_at.is_none() {
            ra.ec2_done_at = Some(at);
        }
    }
}

/// Recompute N, the EC2 tiles wanted and every slot's rectangle. Deterministic in the
/// state, so it runs after every event.
///
/// - fcvm GPUs: slots whose host is present and not draining, in slot-number order.
/// - EC2 tiles wanted: `min(ec2_max, max_tiles, fcvm GPUs)`; `ec2-1..ec2-D` exist, higher
///   ones are dropped (the driver terminates their instances).
/// - EC2 instances: EC2 tiles that report an instance.
/// - N = min(max_tiles, max(fcvm GPUs, EC2 instances)); the first N fcvm slots take
///   rectangles 1..N of the left half, `ec2-k` takes rectangle k of the right half, and
///   the rest of each half is dark. fcvm slots beyond N are standby.
fn retile(f: &mut Fleet, c: &Config, at: u64) {
    let active: Vec<u32> = f
        .slots
        .iter()
        .filter(|(_, s)| {
            s.host
                .as_ref()
                .and_then(|h| f.hosts.get(h))
                .is_some_and(|h| !h.draining)
        })
        .map(|(n, _)| *n)
        .collect();
    let want_ec2 = c.ec2_max.min(c.max_tiles).min(active.len()) as u32;
    f.ec2.retain(|n, _| *n <= want_ec2);
    for n in 1..=want_ec2 {
        f.ec2
            .entry(n)
            .or_insert_with(|| new_slot(ec2_name(n), Half::Ec2, None, None, at));
    }
    let instances = f.ec2.values().filter(|s| s.observed.vm.is_some()).count();
    let n = c.max_tiles.min(active.len().max(instances));
    f.n = n;
    let left = half_rects(Half::Fcvm, n, c.image_aspect, c.tile_aspect);
    let right = half_rects(Half::Ec2, n, c.image_aspect, c.tile_aspect);
    for s in f.slots.values_mut() {
        s.position = None;
        s.rect = None;
    }
    for (k, num) in active.iter().take(n).enumerate() {
        let s = f.slots.get_mut(num).unwrap();
        s.position = Some(k);
        s.rect = Some(left[k]);
    }
    for (num, s) in f.ec2.iter_mut() {
        let k = (*num - 1) as usize;
        s.position = (k < n).then_some(k);
        s.rect = right.get(k).copied();
    }
    drop_missing_pending(f, at);
}
