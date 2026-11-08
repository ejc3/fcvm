// The mock fleet's Windows sandboxes (PROTOCOL.md, "Windows mode"): sandbox slots with a stream
// each, the four intents with the coordinator's rules and refusal texts (reset one or all, borrow
// with a cap, a cooldown and one sandbox per client, fixed actions one a second), and a reset
// when a borrow ends. The coordinator keys the per-client rules by client address; the mock keys
// them by connection, so one machine can play several viewers.

import type { Rect } from "../core/protocol.ts";
import type { PhonesHost, Reply } from "./phones.ts";

export const WINDOWS_LABEL = "Windows Server 2025 evaluation, for demonstration only";
export const WINDOWS_ACTION_NAMES = ["start_menu", "notepad", "explorer", "task_manager", "about", "terminal", "close_all", "reset"] as const;
type WindowsAction = (typeof WINDOWS_ACTION_NAMES)[number];

/** One action a second per borrowed sandbox. */
const ACTION_INTERVAL_MS = 1000;
/** One reset of one sandbox per client every 2 s, and one reset of all every 10 s (`relaunch_interval_ms`). */
const RESET_INTERVAL_MS = 2000;
const RESET_ALL_INTERVAL_MS = 10_000;
/** From 150 days after the installation the image warns; from 170 the agent refuses it. */
const WARN_DAYS = 150;
const REFUSE_DAYS = 170;

export interface MockWindowsOptions {
  count: number;
  /** Sandboxes lent at once at most. */
  cap: number;
  /** How long a borrow lasts. */
  claimSecs: number;
  /** After a sandbox comes back, its viewer waits this long to borrow again. */
  cooldownSecs: number;
  /** A sandbox's restore, click to the first frame of its desktop. */
  restoreMs: number;
  /** The image's age in whole days (12). From 150 it warns, from 170 it is refused. */
  imageAgeDays?: number;
}

interface Borrow {
  session: string;
  viewer: string;
  since: number;
  until: number;
  lastActionAt: number | null;
  timer: NodeJS.Timeout;
}

type Phase = "running" | "restoring" | "connecting";

interface Sandbox {
  n: number;
  id: string;
  slot: string;
  rect: Rect;
  phase: Phase;
  generation: number;
  lastRelaunch: { clicked_at: number; by: string; first_frame_at: number | null; phases: Record<string, number> };
  borrow: Borrow | null;
  timers: NodeJS.Timeout[];
  /** The actions its host's agent lists in its hello, and `reset`, which is the coordinator's own. */
  offers: WindowsAction[];
}

/** An intent of the four, as the mock received it, with its outcome. */
export interface RecordedWindowsIntent {
  at: number;
  viewer: string;
  name: string;
  args: Record<string, unknown>;
  outcome: "committed" | "refused";
}

export interface RecordedWindowsAction {
  at: number;
  viewer: string;
  session: string;
  sandbox: string;
  action: WindowsAction;
}

/** The sandboxes' image: one row of up to four, then rows of equal length, each 1280 x 720. */
export function windowsGrid(n: number): { cols: number; rows: number; aspect: number } {
  const rows = Math.ceil(n / 4);
  const cols = Math.ceil(n / rows);
  return { cols, rows, aspect: (cols * 1280) / (rows * 720) };
}

const INTENTS = new Set(["windows_reset", "claim_windows", "release_windows", "windows_action"]);

export class MockWindows {
  readonly opts: MockWindowsOptions;
  private sandboxes: Sandbox[] = [];
  private host: PhonesHost;
  private claims = 0;
  private lastEnd = new Map<string, number>();
  private lastReset = new Map<string, number>();
  private lastResetAll = new Map<string, number>();
  private resetAll: { clicked_at: number; by: string; sandboxes: number } | null = null;
  readonly actions: RecordedWindowsAction[] = [];
  readonly intents: RecordedWindowsIntent[] = [];
  readonly aspect: number;

  constructor(opts: MockWindowsOptions, host: PhonesHost, epochNow: number) {
    this.opts = opts;
    this.host = host;
    const g = windowsGrid(opts.count);
    this.aspect = g.aspect;
    for (let i = 0; i < opts.count; i++) {
      const n = i + 1;
      const rect: Rect = { x: (i % g.cols) / g.cols, y: Math.floor(i / g.cols) / g.rows, w: 1 / g.cols, h: 1 / g.rows };
      const total = 2400 + n * 130;
      this.sandboxes.push({
        n,
        id: `win:${n}`,
        slot: `win-${n}`,
        rect,
        phase: "running",
        generation: 1,
        lastRelaunch: {
          clicked_at: epochNow - total - 3000,
          by: "start",
          first_frame_at: epochNow - 3000,
          phases: phasesOf(total),
        },
        borrow: null,
        timers: [],
        offers: [...WINDOWS_ACTION_NAMES],
      });
      host.renderers.setTile(`win-${n}`, rect, () => [], () => ({ ...host.scene(), image_aspect: g.aspect }));
      host.renderers.setAvailable(`win-${n}`, true);
    }
  }

  get count(): number {
    return this.sandboxes.length;
  }

  private image() {
    const age = this.opts.imageAgeDays ?? 12;
    return {
      id: "3f9c0a1b2c4d",
      edition: "Windows Server 2025 Datacenter Evaluation (Desktop Experience)",
      build: "26100.1742",
      installed_at: "2026-11-07T10:00:00Z",
      eval_expires: "2027-05-06T10:00:00+00:00",
      age_days: age,
      warning: age >= WARN_DAYS ? `This Windows image is ${age} days old. Build a new one before its evaluation ends.` : null,
      refused: age >= REFUSE_DAYS ? `This Windows image is ${age} days old, so no new sandbox starts from it.` : null,
      gold: "ready",
      gold_error: null,
    };
  }

  state() {
    const lent = this.sandboxes.filter((p) => p.borrow !== null);
    return {
      label: WINDOWS_LABEL,
      image_aspect: this.aspect,
      claim_secs: this.opts.claimSecs,
      borrow: { cap: this.opts.cap, lent: lent.length, next_return_at: lent.length === 0 ? null : Math.min(...lent.map((p) => p.borrow!.until)) },
      image: this.image(),
      last_reset_all: this.resetAll,
      tiles: this.sandboxes.map((p) => ({
        id: p.id,
        half: "fcvm",
        rect: p.rect,
        phase: p.phase,
        slot: p.slot,
        vm: `fleet-${p.slot}`,
        generation: p.generation,
        fps: p.phase === "running" ? 30 : null,
        error: null,
        failure: null,
        frame_url: p.phase === "running" ? this.host.renderers.frameUrl(p.slot) : null,
        frame_url_v4: p.phase === "running" ? this.host.renderers.frameUrlV4(p.slot) : null,
        gpu: null,
        host: `m5.metal · mock-host-${1 + Math.floor((p.n - 1) / 8)}`,
        place: null,
        last_relaunch: p.lastRelaunch,
        borrowed: p.borrow && { session: p.borrow.session, viewer: p.borrow.viewer, since: p.borrow.since, until: p.borrow.until },
        actions: [...p.offers],
      })),
    };
  }

  private own(viewer: string): Sandbox | null {
    return this.sandboxes.find((p) => p.borrow?.viewer === viewer) ?? null;
  }

  private free(p: Sandbox): string | null {
    if (p.borrow) return `${p.slot} is lent to another viewer`;
    if (p.phase !== "running") return `${p.slot} is not running`;
    return null;
  }

  /** Run one Windows intent. Returns undefined for a name that is not one. */
  apply<R>(viewer: string, name: string, args: Record<string, unknown>, h: Reply<R>): R | undefined {
    if (!INTENTS.has(name)) return undefined;
    const r = this.run(viewer, name, args, h);
    this.intents.push({ at: this.host.now(), viewer, name, args, outcome: this.lastOutcome });
    return r;
  }

  private lastOutcome: "committed" | "refused" = "refused";

  private run<R>(viewer: string, name: string, args: Record<string, unknown>, h0: Reply<R>): R {
    const h: Reply<R> = {
      refuse: (reason) => {
        this.lastOutcome = "refused";
        return h0.refuse(reason);
      },
      committed: (summary) => {
        this.lastOutcome = "committed";
        return h0.committed(summary);
      },
    };
    const now = this.host.now();
    const later = (ms: number, fn: () => void) => this.host.later(ms, fn);
    switch (name) {
      case "windows_reset": {
        const slot = args["slot"];
        const all = args["all"];
        if (slot !== undefined && slot !== null && typeof slot !== "string") return h.refuse("args.slot must be a sandbox slot");
        if (all !== undefined && all !== null && typeof all !== "boolean") return h.refuse("args.all must be true");
        const hasSlot = typeof slot === "string";
        const wantsAll = all === true;
        if (hasSlot === wantsAll) return h.refuse("args must name a slot or say all");
        if (wantsAll) {
          const last = this.lastResetAll.get(viewer);
          if (last !== undefined && now < last + RESET_ALL_INTERVAL_MS) return h.refuse("one reset all per 10 s per client");
          const targets = this.sandboxes.filter((p) => p.borrow === null);
          if (targets.length === 0) return h.refuse("no sandbox can be reset now");
          this.lastResetAll.set(viewer, now);
          this.resetAll = { clicked_at: now, by: `viewer:${viewer}`, sandboxes: targets.length };
          for (const p of targets) this.relaunch(p, "reset_all");
          return h.committed(`reset all (${targets.length} sandboxes)`);
        }
        const last = this.lastReset.get(viewer);
        if (last !== undefined && now < last + RESET_INTERVAL_MS) return h.refuse("one reset per 2 s per client");
        const p = this.sandboxes.find((x) => x.slot === slot);
        if (!p) return h.refuse(`no sandbox "${String(slot)}"`);
        if (p.borrow) return h.refuse(`${p.slot} is lent to a viewer`);
        this.lastReset.set(viewer, now);
        this.relaunch(p, `viewer:${viewer}`);
        return h.committed(`reset ${p.slot}`);
      }
      case "claim_windows": {
        const want = args["slot"];
        if (want !== undefined && want !== null && typeof want !== "string") return h.refuse("args.slot must be a sandbox slot");
        if (this.own(viewer)) return h.refuse("one sandbox per client at a time");
        const end = this.lastEnd.get(viewer);
        if (end !== undefined && now < end + this.opts.cooldownSecs * 1000) {
          const wait = Math.ceil((end + this.opts.cooldownSecs * 1000 - now) / 1000);
          return h.refuse(`your last sandbox was returned ${Math.floor((now - end) / 1000)} s ago; borrow again in ${wait} s`);
        }
        const lent = this.sandboxes.filter((p) => p.borrow);
        if (lent.length >= this.opts.cap) {
          const back = Math.ceil(Math.max(0, Math.min(...lent.map((p) => p.borrow!.until)) - now) / 1000);
          return h.refuse(`${lent.length} of ${this.opts.cap} sandboxes are already lent; the next comes back in ${back} s`);
        }
        let sandbox: Sandbox | undefined;
        if (typeof want === "string") {
          sandbox = this.sandboxes.find((p) => p.slot === want);
          if (!sandbox) return h.refuse(`no sandbox "${want}"`);
          const why = this.free(sandbox);
          if (why) return h.refuse(why);
        } else {
          sandbox = this.sandboxes.find((p) => this.free(p) === null);
          if (!sandbox) return h.refuse("no sandbox can be lent now: none is running and free");
        }
        const session = `windows-claim-${++this.claims}`;
        const target = sandbox;
        const timer = later(this.opts.claimSecs * 1000, () => this.release(target));
        sandbox.borrow = { session, viewer, since: now, until: now + this.opts.claimSecs * 1000, lastActionAt: null, timer };
        return h.committed(`${session} on ${sandbox.slot}`);
      }
      case "release_windows": {
        const claim = args["claim"];
        if (claim !== undefined && claim !== null && typeof claim !== "string") return h.refuse("args.claim must be a claim id");
        const p = this.own(viewer);
        if (!p) return h.refuse("this viewer holds no sandbox");
        if (typeof claim === "string" && claim !== p.borrow!.session) return h.refuse(`"${claim}" is not this viewer's sandbox`);
        this.release(p);
        return h.committed("release sandbox");
      }
      case "windows_action": {
        const action = WINDOWS_ACTION_NAMES.find((a) => a === args["action"]);
        if (!action) return h.refuse(`args.action must be one of ${WINDOWS_ACTION_NAMES.join(", ")}`);
        // no point, key or text: nothing but the action and the claim
        for (const k of Object.keys(args)) if (k !== "action" && k !== "claim") return h.refuse(`args.${k} is not accepted`);
        const claim = args["claim"];
        if (claim !== undefined && claim !== null && typeof claim !== "string") return h.refuse("args.claim must be a claim id");
        const p = this.own(viewer);
        if (!p) return h.refuse("sandbox actions need a sandbox of your own (claim_windows)");
        const b = p.borrow!;
        if (typeof claim === "string" && claim !== b.session) return h.refuse(`"${claim}" is not this viewer's sandbox`);
        if (!p.offers.includes(action)) return h.refuse(`${p.slot}'s host does not support ${action}`);
        if (b.lastActionAt !== null && now < b.lastActionAt + ACTION_INTERVAL_MS) return h.refuse(`one action a second; wait ${b.lastActionAt + ACTION_INTERVAL_MS - now} ms`);
        b.lastActionAt = now;
        if (action === "reset") {
          this.relaunch(p, `reset:${b.session}`);
          return h.committed(`reset ${p.slot}`);
        }
        this.actions.push({ at: now, viewer, session: b.session, sandbox: p.slot, action });
        return h.committed(`${action} on ${p.slot}`);
      }
      default:
        throw new Error(`not a Windows intent: ${name}`);
    }
  }

  /** A viewer left: its sandbox goes back. */
  left(viewer: string): void {
    const p = this.own(viewer);
    if (p) this.release(p);
  }

  /** A borrow ends: the sandbox is restored from the snapshot, so the next viewer finds the idle desktop. */
  private release(p: Sandbox): void {
    const b = p.borrow;
    if (!b) return;
    clearTimeout(b.timer);
    p.borrow = null;
    this.lastEnd.set(b.viewer, this.host.now());
    this.relaunch(p, `return:${b.session}`);
  }

  /** restoring, then connecting, then running with the new restore's timings. */
  private relaunch(p: Sandbox, by: string): void {
    const h = this.host;
    for (const t of p.timers) clearTimeout(t);
    p.timers = [];
    p.generation += 1;
    const gen = p.generation;
    const clicked = h.now();
    const total = this.opts.restoreMs;
    p.phase = "restoring";
    p.lastRelaunch = { clicked_at: clicked, by, first_frame_at: null, phases: {} };
    h.renderers.setAvailable(p.slot, false);
    h.markDirty();
    p.timers.push(
      h.later(Math.round(total * 0.6), () => {
        if (p.generation !== gen) return;
        p.phase = "connecting";
        h.markDirty();
      }),
    );
    p.timers.push(
      h.later(total, () => {
        if (p.generation !== gen) return;
        const done = h.now();
        p.phase = "running";
        p.timers = [];
        p.lastRelaunch = { clicked_at: clicked, by, first_frame_at: done, phases: phasesOf(done - clicked) };
        h.renderers.setAvailable(p.slot, true);
        h.markDirty();
      }),
    );
  }

  /** What one sandbox's host lists in its hello (`reset` is always offered). */
  setOffers(slot: string, offers: readonly string[]): void {
    const p = this.sandboxes.find((x) => x.slot === slot);
    if (!p) throw new Error(`no sandbox ${slot}`);
    const bad = offers.filter((a) => !WINDOWS_ACTION_NAMES.some((n) => n === a));
    if (bad.length) throw new Error(`not Windows actions: ${bad.join(", ")}`);
    p.offers = WINDOWS_ACTION_NAMES.filter((n) => n === "reset" || offers.includes(n));
    this.host.markDirty();
  }

  /** The image's age, for the warning and the refusal. */
  setImageAge(days: number): void {
    (this.opts as { imageAgeDays?: number }).imageAgeDays = days;
    this.host.markDirty();
  }

  /** Tests: put the sandboxes back as a fresh viewer finds them. */
  reset(over: { release_all?: boolean; image_age_days?: number }): void {
    if (over.release_all) {
      for (const p of this.sandboxes) {
        if (p.borrow) clearTimeout(p.borrow.timer);
        p.borrow = null;
      }
    }
    if (typeof over.image_age_days === "number") (this.opts as { imageAgeDays?: number }).imageAgeDays = over.image_age_days;
    this.lastEnd.clear();
    this.lastReset.clear();
    this.lastResetAll.clear();
    this.resetAll = null;
    this.actions.length = 0;
    this.intents.length = 0;
    for (const p of this.sandboxes) p.offers = [...WINDOWS_ACTION_NAMES];
    this.host.markDirty();
  }

  close(): void {
    for (const p of this.sandboxes) {
      for (const t of p.timers) clearTimeout(t);
      if (p.borrow) clearTimeout(p.borrow.timer);
    }
  }
}

function phasesOf(total: number): Record<string, number> {
  return {
    restore_ms: Math.round(total * 0.6),
    connect_ms: Math.round(total * 0.25),
    first_frame_ms: Math.round(total * 0.1),
    desktop_ms: Math.round(total * 0.05),
    total_ms: total,
  };
}
