// The mock fleet's phones (PROTOCOL.md, "Phones mode"): phone slots with a stream each, the
// borrow rules the coordinator applies (one phone per viewer, a cap, a cooldown, a fixed set
// of actions, one a second) and the zoom every viewer shares. The coordinator keys the
// one-per-client and cooldown rules by client address; the mock keys them by connection, so one
// machine can play several viewers.

import type { Rect } from "../core/protocol.ts";
import type { MockRenderers } from "./renderer.ts";
import type { PatternScene } from "./pattern.ts";

export const PHONE_ACTION_NAMES = ["switch_screen", "rotate", "dark_mode", "dance", "wave", "gpu_benchmark", "restart_app", "reboot_phone"] as const;
type PhoneAction = (typeof PHONE_ACTION_NAMES)[number];

/** One action a second per borrowed phone. */
const ACTION_INTERVAL_MS = 1000;

export interface MockPhonesOptions {
  count: number;
  /** Phones lent at once at most. */
  cap: number;
  /** How long a borrow lasts. */
  claimSecs: number;
  /** After a phone comes back, its viewer waits this long to borrow again. */
  cooldownSecs: number;
  /** A phone's restore (a reboot, or a return), click to first frame. */
  restoreMs: number;
  /** A viewer's zoom is refused when sent sooner than this after its last. */
  zoomGapMs: number;
}

interface Borrow {
  session: string;
  viewer: string;
  since: number;
  until: number;
  lastActionAt: number | null;
  timer: NodeJS.Timeout;
}

interface Phone {
  n: number;
  id: string;
  slot: string;
  rect: Rect;
  phase: "running" | "restoring";
  generation: number;
  lastRelaunch: { clicked_at: number; by: string; first_frame_at: number | null; phases: Record<string, number> };
  borrow: Borrow | null;
  timer: NodeJS.Timeout | null;
}

/** What the phones need of the coordinator that holds them. */
export interface PhonesHost {
  now(): number;
  later(ms: number, fn: () => void): NodeJS.Timeout;
  /** Publish a change; returns the version it will carry. */
  markDirty(): number;
  scene(): PatternScene;
  renderers: MockRenderers;
}

/** How an intent is answered: the coordinator's receipt for a refusal or an acceptance. */
export interface Reply<R> {
  refuse(reason: string): R;
  committed(summary: string): R;
}

export interface RecordedPhoneAction {
  at: number;
  viewer: string;
  session: string;
  phone: string;
  action: PhoneAction;
}

/** The phones' image: one row of up to six, then rows of equal length. */
export function phonesGrid(n: number): { cols: number; rows: number; aspect: number } {
  const rows = Math.ceil(n / 6);
  const cols = Math.ceil(n / rows);
  return { cols, rows, aspect: (cols * 720) / (rows * 1280) };
}

export class MockPhones {
  readonly opts: MockPhonesOptions;
  private phones: Phone[] = [];
  private host: PhonesHost;
  private claims = 0;
  private lastEnd = new Map<string, number>();
  private lastZoom = new Map<string, number>();
  private zoomedOut = false;
  private zoomAt: number | null = null;
  readonly actions: RecordedPhoneAction[] = [];
  readonly aspect: number;

  constructor(opts: MockPhonesOptions, host: PhonesHost, epochNow: number) {
    this.opts = opts;
    this.host = host;
    const g = phonesGrid(opts.count);
    this.aspect = g.aspect;
    for (let i = 0; i < opts.count; i++) {
      const n = i + 1;
      const rect: Rect = { x: (i % g.cols) / g.cols, y: Math.floor(i / g.cols) / g.rows, w: 1 / g.cols, h: 1 / g.rows };
      // Restored some time ago, each in its own time.
      const total = 20_000 + n * 1300;
      this.phones.push({
        n,
        id: `phone:${n}`,
        slot: `phone-${n}`,
        rect,
        phase: "running",
        generation: 1,
        lastRelaunch: {
          clicked_at: epochNow - total - 5000,
          by: "start",
          first_frame_at: epochNow - 5000,
          phases: { restore_ms: Math.round(total * 0.62), container_ms: Math.round(total * 0.25), first_frame_ms: Math.round(total * 0.13), total_ms: total },
        },
        borrow: null,
        timer: null,
      });
      host.renderers.setTile(`phone-${n}`, rect, () => [], () => ({ ...host.scene(), image_aspect: g.aspect }));
      host.renderers.setAvailable(`phone-${n}`, true);
    }
  }

  get count(): number {
    return this.phones.length;
  }

  state() {
    const lent = this.phones.filter((p) => p.borrow !== null);
    return {
      image_aspect: this.aspect,
      claim_secs: this.opts.claimSecs,
      borrow: { cap: this.opts.cap, lent: lent.length, next_return_at: lent.length === 0 ? null : Math.min(...lent.map((p) => p.borrow!.until)) },
      actions: [...PHONE_ACTION_NAMES],
      zoomed_out: this.zoomedOut,
      zoom_at: this.zoomAt,
      tiles: this.phones.map((p) => ({
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
        gpu: { sbdf: `0000:${(0x30 + p.n).toString(16)}:00.0`, name: "Tesla T4" },
        gpu_stats: { temp_c: 52, power_w: 48, util_pct: 37 },
        host: `g4dn.metal · mock-host-${1 + Math.floor((p.n - 1) / 8)}`,
        place: null,
        last_relaunch: p.lastRelaunch,
        borrowed: p.borrow && { session: p.borrow.session, viewer: p.borrow.viewer, since: p.borrow.since, until: p.borrow.until },
      })),
    };
  }

  private own(viewer: string): Phone | null {
    return this.phones.find((p) => p.borrow?.viewer === viewer) ?? null;
  }

  private free(p: Phone): string | null {
    if (p.borrow) return `${p.slot} is lent to another viewer`;
    if (p.phase !== "running") return `${p.slot} is not running`;
    return null;
  }

  /** Run one phone intent. Returns undefined for a name that is not a phone intent. */
  apply<R>(viewer: string, name: string, args: Record<string, unknown>, h: Reply<R>): R | undefined {
    const now = this.host.now();
    const later = (ms: number, fn: () => void) => this.host.later(ms, fn);
    switch (name) {
      case "claim_phone": {
        if (this.own(viewer)) return h.refuse("one phone per client at a time");
        const end = this.lastEnd.get(viewer);
        if (end !== undefined && now < end + this.opts.cooldownSecs * 1000) {
          const wait = Math.ceil((end + this.opts.cooldownSecs * 1000 - now) / 1000);
          return h.refuse(`your last phone was returned ${Math.floor((now - end) / 1000)} s ago; borrow again in ${wait} s`);
        }
        const lent = this.phones.filter((p) => p.borrow);
        if (lent.length >= this.opts.cap) {
          const back = Math.ceil(Math.max(0, Math.min(...lent.map((p) => p.borrow!.until)) - now) / 1000);
          return h.refuse(`${lent.length} of ${this.opts.cap} phones are already lent; the next comes back in ${back} s`);
        }
        const want = args["phone"];
        if (want !== undefined && want !== null && typeof want !== "string") return h.refuse("args.phone must be a phone id");
        let phone: Phone | undefined;
        if (typeof want === "string") {
          phone = this.phones.find((p) => p.slot === want);
          if (!phone) return h.refuse(`no phone "${want}"`);
          const why = this.free(phone);
          if (why) return h.refuse(why);
        } else {
          phone = this.phones.find((p) => this.free(p) === null);
          if (!phone) return h.refuse("no phone can be lent now: none is running and free");
        }
        const session = `phone-claim-${++this.claims}`;
        const target = phone;
        const timer = later(this.opts.claimSecs * 1000, () => this.release(target, "return"));
        phone.borrow = { session, viewer, since: now, until: now + this.opts.claimSecs * 1000, lastActionAt: null, timer };
        return h.committed(`${session} on ${phone.slot}`);
      }
      case "release_phone": {
        const p = this.own(viewer);
        if (!p) return h.refuse("this viewer holds no phone");
        const claim = args["claim"];
        if (claim !== undefined && claim !== null && typeof claim !== "string") return h.refuse("args.claim must be a claim id");
        if (typeof claim === "string" && claim !== p.borrow!.session) return h.refuse(`"${claim}" is not this viewer's phone`);
        this.release(p, "return");
        return h.committed("release phone");
      }
      case "phone_action": {
        const action = PHONE_ACTION_NAMES.find((a) => a === args["action"]);
        if (!action) return h.refuse(`args.action must be one of ${PHONE_ACTION_NAMES.join(", ")}`);
        const claim = args["claim"];
        if (claim !== undefined && claim !== null && typeof claim !== "string") return h.refuse("args.claim must be a claim id");
        const p = this.own(viewer);
        if (!p) return h.refuse("phone actions need a phone of your own (claim_phone)");
        const b = p.borrow!;
        if (typeof claim === "string" && claim !== b.session) return h.refuse(`"${claim}" is not this viewer's phone`);
        if (b.lastActionAt !== null && now < b.lastActionAt + ACTION_INTERVAL_MS) return h.refuse(`one action a second; wait ${b.lastActionAt + ACTION_INTERVAL_MS - now} ms`);
        b.lastActionAt = now;
        if (action === "reboot_phone") {
          this.relaunch(p, `reboot:${b.session}`);
          return h.committed(`reboot ${p.slot}`);
        }
        this.actions.push({ at: now, viewer, session: b.session, phone: p.slot, action });
        return h.committed(`${action} on ${p.slot}`);
      }
      case "phones_zoom": {
        if (typeof args["out"] !== "boolean") return h.refuse("args.out must be true or false");
        const last = this.lastZoom.get(viewer);
        if (last !== undefined && now < last + this.opts.zoomGapMs) return h.refuse(`one zoom every ${this.opts.zoomGapMs / 1000} s; wait ${last + this.opts.zoomGapMs - now} ms`);
        this.lastZoom.set(viewer, now);
        if (args["out"] !== this.zoomedOut) {
          this.zoomedOut = args["out"];
          this.zoomAt = now;
        }
        return h.committed(`zoom ${this.zoomedOut ? "out" : "in"}`);
      }
      default:
        return undefined;
    }
  }

  /** A viewer left: its phone goes back. */
  left(viewer: string): void {
    const p = this.own(viewer);
    if (p) this.release(p, "return");
  }

  /** A phone comes back: restored from its snapshot, so the next viewer finds it as every phone starts. */
  private release(p: Phone, why: string): void {
    const b = p.borrow;
    if (!b) return;
    clearTimeout(b.timer);
    p.borrow = null;
    this.lastEnd.set(b.viewer, this.host.now());
    this.relaunch(p, `${why}:${b.session}`);
  }

  private relaunch(p: Phone, by: string): void {
    const h = this.host;
    if (p.timer) clearTimeout(p.timer);
    p.generation += 1;
    const gen = p.generation;
    const clicked = h.now();
    p.phase = "restoring";
    p.lastRelaunch = { clicked_at: clicked, by, first_frame_at: null, phases: {} };
    h.renderers.setAvailable(p.slot, false);
    h.markDirty();
    p.timer = h.later(this.opts.restoreMs, () => {
      if (p.generation !== gen) return;
      const done = h.now();
      const total = done - clicked;
      p.phase = "running";
      p.timer = null;
      p.lastRelaunch = {
        clicked_at: clicked,
        by,
        first_frame_at: done,
        phases: { restore_ms: Math.round(total * 0.7), container_ms: Math.round(total * 0.2), first_frame_ms: Math.round(total * 0.1), total_ms: total },
      };
      h.renderers.setAvailable(p.slot, true);
      h.markDirty();
    });
  }

  /** Tests: put the phones back as a fresh viewer finds them. */
  reset(over: { zoomed_out?: boolean; release_all?: boolean }): void {
    if (typeof over.zoomed_out === "boolean" && over.zoomed_out !== this.zoomedOut) {
      this.zoomedOut = over.zoomed_out;
      this.zoomAt = this.host.now();
    }
    if (over.release_all) {
      for (const p of this.phones) {
        if (p.borrow) clearTimeout(p.borrow.timer);
        p.borrow = null;
      }
    }
    this.lastEnd.clear();
    this.lastZoom.clear();
    this.actions.length = 0;
    this.host.markDirty();
  }

  close(): void {
    for (const p of this.phones) {
      if (p.timer) clearTimeout(p.timer);
      if (p.borrow) clearTimeout(p.borrow.timer);
    }
  }
}
