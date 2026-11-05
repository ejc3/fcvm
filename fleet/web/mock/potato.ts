// The mock fleet's potato game (PROTOCOL.md, "Potato mode"; fleet/core/src/potato.rs): one
// host's players passing a ball, sleeping after each throw and woken when the ball is near,
// the ladder of ring sizes, drops, records, and the two rate-limited intents. Times are drawn
// from the measured ones (docs/fleet/ARCHITECTURE.md, "Measured: one player's sleep and
// wake"): a sleep of about 350 ms, a wake of about 350 ms, and now and then a 1.5 s wake.
// The coordinator keys its rate limits by client address; the mock keys them by connection.

/** A small deterministic generator, so a mock run is repeatable. */
function rng(seed: number): () => number {
  let s = seed >>> 0;
  return () => {
    s = (s * 1664525 + 1013904223) >>> 0;
    return s / 2 ** 32;
  };
}

export interface MockPotatoOptions {
  /** The players the host runs (its `potato_players`). */
  players: number;
  sizes?: number[];
  /** A ring of n starts at max(budgetMs / (n - 1), startFloorMs) of hold. */
  budgetMs: number;
  startFloorMs: number;
  /** After a drop, the next round starts this long after. */
  restartAfterMs: number;
  sleepMs: number;
  wakeMs: number;
  /** The occasional slow wake, and how often one comes (0 to 1). */
  slowWakeMs: number;
  slowWakeChance: number;
  seed?: number;
}

const SIZES = [2, 4, 8, 16, 32, 64, 96];
const SHRINK_PERMILLE = 100;
const MIN_HOLD_MS = 2;
const FLIGHT_MS = 100;
const COLOURS = 12;
const FASTER_INTERVAL_MS = 2000;
const RESTART_INTERVAL_MS = 10_000;
/** The agent wakes a sleeping player this long, plus its slowest recent wake, before the ball is due. */
const WAKE_MARGIN_MS = 120;
/** The first ball reaches the first player this long after the round starts. */
const KICKOFF_MS = 200;

type Phase = "awake" | "asleep" | "waking" | "failed";

interface Player {
  colour: number;
  counter: number;
  phase: Phase;
  since: number;
  /** Thrown and being put to sleep: the view still says awake, as the coordinator's does until `slept`. */
  sleepingSince: number | null;
  last_sleep_ms: number | null;
  last_wake_ms: number | null;
  sleep_stages: Record<string, number>;
  wake_stages: Record<string, number>;
  timer: NodeJS.Timeout | null;
  wakeTimer: NodeJS.Timeout | null;
}

interface Drop {
  at: number;
  round: number;
  size: number;
  reason: string;
  player: number | null;
  thrower: number | null;
  seq: number;
  lap: number;
  hold_ms: number;
  stage: string;
  stage_ms: number;
  listening_after_ms: number | null;
  sleep_stages: Record<string, number>;
  wake_stages: Record<string, number>;
  detail: string;
}

interface Rung {
  size: number;
  record_hold_ms: number | null;
  record_at: number | null;
  broke: Drop | null;
  rounds: number;
}

interface Round {
  id: number;
  size: number;
  ring: number[];
  started_at: number;
  start_hold_ms: number;
  pace_ms: number | null;
  seq: number | null;
  holder: number | null;
  last_catch_at: number | null;
  flight: { from: number; to: number; at: number } | null;
  best_lap_hold_ms: number | null;
  /** The ball is thrown to a player that does not accept yet: it is caught if it wakes by `until`. */
  waiting: { to: number; seq: number; until: number; timer: NodeJS.Timeout } | null;
  kickoff: boolean;
  timer: NodeJS.Timeout | null;
}

interface Record_ {
  value: number;
  size: number;
  player: number | null;
  at: number;
}

/** What the game needs of the coordinator that holds it. */
export interface PotatoHost {
  now(): number;
  later(ms: number, fn: () => void): NodeJS.Timeout;
  markDirty(): number;
}

/** How an intent is answered: the coordinator's receipt for a refusal or an acceptance. */
export interface PotatoReply<R> {
  refuse(reason: string): R;
  committed(summary: string): R;
}

export class MockPotato {
  readonly opts: MockPotatoOptions;
  private host: PotatoHost;
  private rand: () => number;
  private players = new Map<number, Player>();
  private ladder: Rung[];
  private rung = 0;
  private round: Round | null = null;
  private roundCount = 0;
  private records: { fastest_cycle_ms: Record_ | null; longest_round: Record_ | null } = { fastest_cycle_ms: null, longest_round: null };
  private lastDrop: Drop | null = null;
  private nextRoundAt: number | null = null;
  private startTimer: NodeJS.Timeout | null = null;
  private recentWakes: number[] = [];
  private lastFaster = new Map<string, number>();
  private lastRestart = new Map<string, number>();
  /** Tests: the next wake takes this long, whatever the draw. */
  private forcedWakeMs: number | null = null;

  constructor(opts: MockPotatoOptions, host: PotatoHost) {
    this.opts = opts;
    this.host = host;
    this.rand = rng(opts.seed ?? 4096);
    this.ladder = (opts.sizes ?? SIZES).map((size) => ({ size, record_hold_ms: null, record_at: null, broke: null, rounds: 0 }));
    // the host says hello: the first round starts a second later, as the coordinator's tick starts it
    this.scheduleStart(1000);
  }

  // ---- the snapshot ------------------------------------------------------------------

  private holdAt(r: Round, seq: number): number {
    const laps = Math.floor(seq / Math.max(1, r.ring.length));
    let h = r.start_hold_ms;
    for (let i = 0; i < Math.min(laps, 10_000); i++) h = Math.max(MIN_HOLD_MS, h - Math.floor((h * SHRINK_PERMILLE) / 1000));
    return r.pace_ms === null ? h : Math.min(h, Math.max(r.pace_ms, MIN_HOLD_MS));
  }

  state() {
    const r = this.round;
    const seq = r?.seq ?? 0;
    const hold = r ? this.holdAt(r, seq) : 0;
    return {
      ladder: this.ladder.map((g) => ({ ...g, broke: g.broke && { ...g.broke } })),
      size: r?.size ?? this.ladder[this.rung]?.size ?? null,
      round: r && {
        id: r.id,
        size: r.size,
        ring: [...r.ring],
        started_at: r.started_at,
        seq: r.seq,
        lap: Math.floor(seq / r.ring.length),
        hold_ms: hold,
        start_hold_ms: r.start_hold_ms,
        min_hold_ms: MIN_HOLD_MS,
        flight_ms: FLIGHT_MS,
        pace_ms: r.pace_ms,
        turnaround_ms: hold * (r.size - 1),
        holder: r.holder,
        last_catch_at: r.last_catch_at,
        flight: r.flight && { ...r.flight },
        best_lap_hold_ms: r.best_lap_hold_ms,
      },
      players: [...this.players.entries()]
        .sort(([a], [b]) => a - b)
        // the coordinator shows a player that threw as sleeping until its sleep is done
        .map(([n, p]) => ({
          player: n,
          colour: p.colour,
          counter: p.counter,
          phase: p.sleepingSince !== null ? "sleeping" : p.phase,
          since: p.sleepingSince ?? p.since,
          last_sleep_ms: p.last_sleep_ms,
          last_wake_ms: p.last_wake_ms,
        })),
      records: { fastest_cycle_ms: this.records.fastest_cycle_ms && { ...this.records.fastest_cycle_ms }, longest_round: this.records.longest_round && { ...this.records.longest_round } },
      last_drop: this.lastDrop && { ...this.lastDrop },
      next_round_at: this.nextRoundAt,
      colours: COLOURS,
      capacity: this.opts.players,
    };
  }

  // ---- rounds ------------------------------------------------------------------------

  private scheduleStart(ms: number): void {
    if (this.startTimer) clearTimeout(this.startTimer);
    this.nextRoundAt = this.host.now() + ms;
    this.startTimer = this.host.later(ms, () => this.startRound());
    this.host.markDirty();
  }

  /** The first rung from `from` on (wrapping) whose size the host runs. */
  private nextRung(from: number): number {
    const n = this.ladder.length;
    for (let k = 0; k < n; k++) {
      const i = (from + k) % n;
      if (this.ladder[i]!.size <= this.opts.players) return i;
    }
    return from % n;
  }

  private startRound(): void {
    this.startTimer = null;
    if (this.opts.players < 2) return;
    this.rung = this.nextRung(this.rung);
    const size = this.ladder[this.rung]!.size;
    const now = this.host.now();
    for (let n = 0; n < size; n++) {
      if (!this.players.has(n)) {
        // a new clone says hello with its first colour, drawn from the guest's random pool
        this.players.set(n, {
          colour: Math.floor(this.rand() * COLOURS),
          counter: 0,
          phase: "awake",
          since: now,
          sleepingSince: null,
          last_sleep_ms: null,
          last_wake_ms: null,
          sleep_stages: {},
          wake_stages: {},
          timer: null,
          wakeTimer: null,
        });
      }
    }
    this.roundCount += 1;
    this.round = {
      id: this.roundCount,
      size,
      ring: Array.from({ length: size }, (_, i) => i),
      started_at: now,
      start_hold_ms: Math.max(Math.floor(this.opts.budgetMs / Math.max(1, size - 1)), this.opts.startFloorMs, MIN_HOLD_MS),
      pace_ms: null,
      seq: null,
      holder: null,
      last_catch_at: null,
      flight: null,
      best_lap_hold_ms: null,
      waiting: null,
      kickoff: true,
      timer: null,
    };
    this.nextRoundAt = null;
    this.host.markDirty();
    const r = this.round;
    r.timer = this.host.later(KICKOFF_MS, () => this.kickoff(r));
  }

  /** The agent serves the round's first ball to its first player, once that player accepts. */
  private kickoff(r: Round): void {
    if (this.round !== r) return;
    r.timer = null;
    const first = r.ring[0]!;
    const p = this.players.get(first)!;
    if (this.accepts(p)) {
      r.kickoff = false;
      this.caught(r, first, 0);
    } else if (p.phase === "asleep") {
      this.wake(first);
    }
  }

  private accepts(p: Player): boolean {
    return p.phase === "awake" && p.sleepingSince === null;
  }

  private caught(r: Round, player: number, seq: number): void {
    const now = this.host.now();
    const p = this.players.get(player)!;
    p.colour = (p.colour + 1) % COLOURS;
    p.counter += 1;
    const n = r.ring.length;
    if (seq > 0 && seq % n === 0) {
      const h = this.holdAt(r, seq - 1);
      if (r.best_lap_hold_ms === null || h < r.best_lap_hold_ms) r.best_lap_hold_ms = h;
    }
    r.seq = seq;
    r.holder = player;
    r.last_catch_at = now;
    r.flight = null;
    r.waiting = null;
    this.host.markDirty();
    const hold = this.holdAt(r, seq);
    r.timer = this.host.later(hold, () => this.threw(r, player, seq + 1));
    this.planWakes(r);
  }

  private threw(r: Round, from: number, seq: number): void {
    if (this.round !== r) return;
    r.timer = null;
    const now = this.host.now();
    const to = r.ring[seq % r.ring.length]!;
    r.flight = { from, to, at: now };
    this.host.markDirty();
    this.sleep(from);
    const p = this.players.get(to)!;
    if (this.accepts(p)) {
      // VM to VM over the host's routes: about a millisecond
      r.timer = this.host.later(1, () => {
        if (this.round === r) this.caught(r, to, seq);
      });
      return;
    }
    // the thrower tries again until the flight window has passed
    const timer = this.host.later(FLIGHT_MS, () => {
      if (this.round !== r || r.waiting?.seq !== seq) return;
      const q = this.players.get(to)!;
      const stage = q.sleepingSince !== null ? "sleeping" : q.phase;
      const since = q.sleepingSince ?? q.since;
      const attempts = Math.floor(FLIGHT_MS / 2);
      this.dropBall(r, {
        reason: "not_listening",
        player: to,
        thrower: from,
        seq,
        stage,
        stage_ms: Math.max(0, this.host.now() - since),
        detail: `player ${to} was ${stage} when player ${from} threw (${attempts} tries in ${FLIGHT_MS} ms: connection refused)`,
      });
    });
    r.waiting = { to, seq, until: now + FLIGHT_MS, timer };
  }

  private dropBall(r: Round, d: Pick<Drop, "reason" | "player" | "thrower" | "seq" | "stage" | "stage_ms" | "detail">): void {
    const now = this.host.now();
    const q = d.player === null ? null : this.players.get(d.player) ?? null;
    const drop: Drop = {
      at: now,
      round: r.id,
      size: r.size,
      reason: d.reason,
      player: d.player,
      thrower: d.thrower,
      seq: d.seq,
      lap: Math.floor(d.seq / r.ring.length),
      hold_ms: this.holdAt(r, d.seq),
      stage: d.stage,
      stage_ms: Math.round(d.stage_ms * 10) / 10,
      listening_after_ms: null,
      sleep_stages: { ...(q?.sleep_stages ?? {}) },
      wake_stages: { ...(q?.wake_stages ?? {}) },
      detail: d.detail,
    };
    this.endRound(r);
    const rung = this.ladder.find((g) => g.size === r.size);
    if (rung) {
      rung.rounds += 1;
      rung.broke = drop;
      if (r.best_lap_hold_ms !== null && (rung.record_hold_ms === null || r.best_lap_hold_ms < rung.record_hold_ms)) {
        rung.record_hold_ms = r.best_lap_hold_ms;
        rung.record_at = now;
      }
    }
    const catches = r.seq === null ? 0 : r.seq + 1;
    if (catches > 0 && (this.records.longest_round === null || catches > this.records.longest_round.value)) {
      this.records.longest_round = { value: catches, size: r.size, player: null, at: now };
    }
    this.lastDrop = drop;
    this.rung = this.nextRung(this.rung + 1);
    this.scheduleStart(this.opts.restartAfterMs);
  }

  private endRound(r: Round): void {
    if (r.timer) clearTimeout(r.timer);
    if (r.waiting) clearTimeout(r.waiting.timer);
    r.timer = null;
    r.waiting = null;
    if (this.round === r) this.round = null;
    // nobody is woken for a round that is over
    for (const p of this.players.values()) {
      if (p.wakeTimer) clearTimeout(p.wakeTimer);
      p.wakeTimer = null;
    }
  }

  // ---- sleep and wake ----------------------------------------------------------------

  private jitter(ms: number): number {
    return Math.round(ms * (0.88 + this.rand() * 0.24) * 10) / 10;
  }

  private sleep(n: number): void {
    const p = this.players.get(n)!;
    if (p.phase !== "awake" || p.sleepingSince !== null) return;
    p.sleepingSince = this.host.now();
    const ms = this.jitter(this.opts.sleepMs);
    p.timer = this.host.later(ms, () => {
      p.timer = null;
      p.sleepingSince = null;
      p.phase = "asleep";
      p.since = this.host.now();
      p.last_sleep_ms = ms;
      const part = (f: number) => Math.round(ms * f * 10) / 10;
      p.sleep_stages = { prepare_ms: part(0.07), pause_ms: part(0.002), save_ms: part(0.29), disk_ms: part(0.33), stop_ms: part(0.31) };
      this.host.markDirty();
      const r = this.round;
      // a round waiting to serve its first ball to this player wakes it now
      if (r && r.kickoff && r.ring[0] === n) this.wake(n);
      else if (r) this.planWakes(r);
    });
  }

  /** The agent's lead: its slowest recent wake plus a margin. */
  private leadMs(): number {
    return Math.max(this.opts.wakeMs, ...this.recentWakes) + WAKE_MARGIN_MS;
  }

  /** Each sleeping player of the ring is woken `lead` before the ball is due at it. */
  private planWakes(r: Round): void {
    if (r.holder === null || r.seq === null || r.last_catch_at === null) return;
    const n = r.ring.length;
    const at = r.ring.indexOf(r.holder);
    const lead = this.leadMs();
    for (let i = 0; i < n; i++) {
      const who = r.ring[i]!;
      const p = this.players.get(who)!;
      if (p.phase !== "asleep" || p.wakeTimer) continue;
      const dist = (i - at + n) % n || n;
      let due = r.last_catch_at;
      for (let k = 0; k < dist; k++) due += this.holdAt(r, r.seq + k);
      const wait = Math.max(0, due - lead - this.host.now());
      p.wakeTimer = this.host.later(wait, () => {
        p.wakeTimer = null;
        if (this.round === r) this.wake(who);
      });
    }
  }

  private wake(n: number): void {
    const p = this.players.get(n)!;
    if (p.phase !== "asleep") return;
    const start = this.host.now();
    p.phase = "waking";
    p.since = start;
    this.host.markDirty();
    let ms = this.rand() < this.opts.slowWakeChance ? this.jitter(this.opts.slowWakeMs) : this.jitter(this.opts.wakeMs);
    if (this.forcedWakeMs !== null) {
      ms = this.forcedWakeMs;
      this.forcedWakeMs = null;
    }
    p.timer = this.host.later(ms, () => {
      p.timer = null;
      const now = this.host.now();
      p.phase = "awake";
      p.since = now;
      p.last_wake_ms = ms;
      p.wake_stages = { restore_ms: Math.round(ms * 3) / 10, ack_ms: Math.round(ms * 5.5) / 10, listen_ms: ms };
      this.recentWakes = [...this.recentWakes, ms].slice(-4);
      const r = this.round;
      if (r && r.ring.includes(n) && p.last_sleep_ms !== null) {
        const cycle = Math.round((p.last_sleep_ms + ms) * 10) / 10;
        if (this.records.fastest_cycle_ms === null || cycle < this.records.fastest_cycle_ms.value) {
          this.records.fastest_cycle_ms = { value: cycle, size: r.size, player: n, at: now };
        }
      }
      // a dropped ball's catcher that did start accepting says when
      const d = this.lastDrop;
      if (d && d.player === n && d.listening_after_ms === null && d.at <= now && (r === null || r.id !== d.round)) {
        d.listening_after_ms = now - d.at;
        const rung = this.ladder.find((g) => g.broke?.round === d.round);
        if (rung?.broke) rung.broke.listening_after_ms = d.listening_after_ms;
      }
      this.host.markDirty();
      if (!r) return;
      if (r.waiting?.to === n) {
        clearTimeout(r.waiting.timer);
        this.caught(r, n, r.waiting.seq);
      } else if (r.kickoff && r.ring[0] === n) {
        r.kickoff = false;
        this.caught(r, n, 0);
      }
    });
  }

  // ---- intents -----------------------------------------------------------------------

  /** Run one potato intent. Returns undefined for a name that is not one. */
  apply<R>(viewer: string, name: string, h: PotatoReply<R>): R | undefined {
    const now = this.host.now();
    switch (name) {
      case "potato_faster": {
        const r = this.round;
        if (!r) return h.refuse("no round is being played");
        const last = this.lastFaster.get(viewer);
        if (last !== undefined && now < last + FASTER_INTERVAL_MS) return h.refuse("one faster every 2 s");
        const cur = this.holdAt(r, r.seq ?? 0);
        if (cur <= MIN_HOLD_MS) return h.refuse("the hold is at its floor");
        r.pace_ms = Math.max(cur - Math.floor(cur / 5), MIN_HOLD_MS);
        this.lastFaster.set(viewer, now);
        return h.committed("faster");
      }
      case "potato_restart": {
        const last = this.lastRestart.get(viewer);
        if (last !== undefined && now < last + RESTART_INTERVAL_MS) return h.refuse("one restart every 10 s");
        const r = this.round;
        if (r && now < r.started_at + RESTART_INTERVAL_MS) return h.refuse("the round has only just started");
        if (!r && this.opts.players < 2) return h.refuse("no players are running");
        if (r) this.endRound(r);
        this.lastRestart.set(viewer, now);
        this.scheduleStart(1000);
        return h.committed("restart");
      }
      default:
        return undefined;
    }
  }

  /**
   * Tests: start over at a size (the ladder's records and the rate limits cleared), the next
   * round `nextInMs` from now, and optionally make the next wake take `slowWakeMs`.
   */
  reset(over: { size?: number; next_in_ms?: number; slow_wake_ms?: number }): void {
    if (this.round) this.endRound(this.round);
    for (const p of this.players.values()) {
      if (p.timer) clearTimeout(p.timer);
      if (p.wakeTimer) clearTimeout(p.wakeTimer);
    }
    this.players.clear();
    this.ladder = this.ladder.map((g) => ({ size: g.size, record_hold_ms: null, record_at: null, broke: null, rounds: 0 }));
    this.records = { fastest_cycle_ms: null, longest_round: null };
    this.lastDrop = null;
    this.recentWakes = [];
    this.lastFaster.clear();
    this.lastRestart.clear();
    if (typeof over.size === "number") {
      const i = this.ladder.findIndex((g) => g.size === over.size);
      if (i < 0) throw new Error(`no ring of ${over.size} on the ladder`);
      this.rung = i;
    }
    this.forcedWakeMs = typeof over.slow_wake_ms === "number" ? over.slow_wake_ms : null;
    this.scheduleStart(over.next_in_ms ?? 500);
  }

  close(): void {
    if (this.startTimer) clearTimeout(this.startTimer);
    if (this.round) this.endRound(this.round);
    for (const p of this.players.values()) {
      if (p.timer) clearTimeout(p.timer);
      if (p.wakeTimer) clearTimeout(p.wakeTimer);
    }
  }
}
