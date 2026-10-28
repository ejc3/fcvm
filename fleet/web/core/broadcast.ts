// The broadcast (docs/fleet/ARCHITECTURE.md, "Broadcast mode"): one IVS stream of the whole
// wall for viewers past the direct capacity, and for anyone who switches to it. The video is
// seconds behind the coordinator, so the overlay is drawn from the state that was current
// when the frame on screen was drawn: each frame carries its scene time t (SEI), and the page
// keeps the snapshots of the last HISTORY_MS with the coordinator time each arrived at.

import type { FleetState } from "./protocol.ts";
import { serverNow, type ClockState } from "./clock.ts";
import type { ClientState } from "./store.ts";

/** How long snapshots are kept for the overlay: well past the broadcast's latency. */
export const HISTORY_MS = 30_000;
/** At most this many: snapshots arrive at most every 50 ms. */
export const HISTORY_MAX = 600;
/**
 * The frame on screen is advanced by the time since it was presented, up to this much: a
 * player that stalls stops presenting frames, and the overlay must stop with it.
 */
export const SHOWN_ADVANCE_MAX_MS = 250;

/** A snapshot and the coordinator time it arrived at. */
export interface HistoryEntry {
  at: number;
  version: number;
  state: FleetState;
}

/** The frame the player presented last: its scene time and epoch, and when (local ms). */
export interface ShownFrame {
  t: number;
  e: number;
  at: number;
}

/** Add a snapshot that arrived at coordinator time `at`, keeping HISTORY_MS of them. */
export function addHistory(h: readonly HistoryEntry[], entry: HistoryEntry): HistoryEntry[] {
  const out = h.filter((x) => x.at >= entry.at - HISTORY_MS && x.version < entry.version);
  out.push(entry);
  return out.length > HISTORY_MAX ? out.slice(out.length - HISTORY_MAX) : out;
}

/** The snapshot in effect at coordinator time `wallMs`: the newest that arrived by then (the oldest kept, if none did). */
export function snapshotAt(h: readonly HistoryEntry[], wallMs: number): HistoryEntry | null {
  let best: HistoryEntry | null = null;
  for (const x of h) if (x.at <= wallMs && (best === null || x.at > best.at)) best = x;
  return best ?? h[0] ?? null;
}

/** The wall time of the frame on screen at local time `localNow`. */
export function frameWallMs(f: ShownFrame, localNow: number): number {
  return f.e + 1000 * f.t + Math.min(Math.max(localNow - f.at, 0), SHOWN_ADVANCE_MAX_MS);
}

/** How far the picture is behind the coordinator, ms. */
export function broadcastLatencyMs(f: ShownFrame, clock: ClockState, localNow: number): number {
  return serverNow(clock, localNow) - frameWallMs(f, localNow);
}

/** Whether this viewer watches the broadcast: forced (a late viewer) or chosen, when there is one. */
export function watchesBroadcast(s: ClientState): boolean {
  const a = s.admission;
  if (a === null || a.broadcast === null) return false;
  return a.locked || s.ui.mode === "broadcast";
}

/**
 * What the overlay draws in broadcast mode: the state that was current at the frame on screen,
 * and a local "now" whose coordinator time is that frame's, so clocks tick with the picture.
 * Before the first frame is presented it is the live state. `pinned` is the entry drawn last:
 * a picture frozen longer than HISTORY_MS (paused, stalled) keeps it after the history has
 * moved past its frame. `entry` is the one drawn now, for the caller to pin.
 */
export function broadcastShown(
  s: ClientState,
  frame: ShownFrame | null,
  localNow: number,
  pinned: HistoryEntry | null = null,
): { state: ClientState; now: number; entry: HistoryEntry | null } {
  if (frame === null || (s.history.length === 0 && pinned === null)) return { state: s, now: localNow, entry: null };
  const wall = frameWallMs(frame, localNow);
  const kept = s.history.some((x) => x.at <= wall);
  const entry = kept || pinned === null ? snapshotAt(s.history, wall) : pinned;
  const now = localNow - (serverNow(s.clock, localNow) - wall);
  if (entry === null) return { state: s, now, entry: null };
  const st = entry.state;
  return {
    state: {
      ...s,
      accepted: st,
      version: entry.version,
      shownLayout: { gen: st.layout.generation, tiles: st.tiles, aggregates: st.aggregates },
    },
    now,
    entry,
  };
}

/**
 * Pairs each presented frame with its SEI, by media time (ms), whichever arrives first: the
 * IVS player can raise a frame's SEI just after the frame is presented.
 */
export class FrameMatcher {
  /** SEIs and presentations kept waiting for their other half (a few seconds at 30 fps). */
  static readonly PENDING_KEPT = 120;
  private seis = new Map<number, { t: number; e: number }>();
  private shown = new Map<number, number>();

  /** An SEI for the frame at `ms`; returns the frame it completes, if that was presented first. */
  sei(ms: number, f: { t: number; e: number }): ShownFrame[] {
    const at = this.shown.get(ms);
    if (at !== undefined) {
      this.shown.delete(ms);
      return [{ ...f, at }];
    }
    this.seis.set(ms, f);
    trim(this.seis);
    return [];
  }

  /** The frame at `ms` was presented at local time `at`; returns it when its SEI is known. */
  presented(ms: number, at: number): ShownFrame | null {
    const f = this.seis.get(ms);
    if (f) return { ...f, at };
    this.shown.set(ms, at);
    trim(this.shown);
    return null;
  }

  pendingCount(): number {
    return this.shown.size;
  }
}

function trim<V>(m: Map<number, V>): void {
  while (m.size > FrameMatcher.PENDING_KEPT) m.delete(m.keys().next().value!);
}

/** The broadcast switch in the drawer. */
export interface BroadcastToggle {
  label: string;
  on: boolean;
  disabled: boolean;
  note: string;
}

export function broadcastToggle(s: ClientState): BroadcastToggle | null {
  const a = s.admission;
  if (a === null || a.broadcast === null) return null;
  if (a.locked) {
    return {
      label: "Watch the broadcast",
      on: true,
      disabled: true,
      note: a.seat_free
        ? `${a.reason ?? "Direct streams are full."} A direct seat is free now: reload the page to take it.`
        : a.reason ?? "Direct streams are full, so you are watching the broadcast.",
    };
  }
  const on = s.ui.mode === "broadcast";
  return {
    label: "Watch the broadcast",
    on,
    disabled: false,
    note: on
      ? "One video of the whole wall from the CDN, a couple of seconds behind. Words, lights and votes still reach the scene."
      : "Switch to one video of the whole wall from the CDN instead of a stream per VM.",
  };
}
