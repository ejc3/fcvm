// Minecraft mode's 3D view: every frame, each tile on screen gets its world drawn into its
// rectangle of the one WebGL canvas under the tiles, from the camera core/minecraft3d.ts gives it,
// and its bots' names on the tile's own canvas above. A fork's picture flies from its source to
// the tile taking the copy. Frames that run long slow the drawing down (FrameBudget); tiles off the
// screen and a hidden tab hold nothing on the GPU.

import {
  displayTick,
  figureBoxes,
  flightBox,
  flightProgress,
  FLIGHT_LAND_MS,
  FrameBudget,
  labelsOf,
  MATERIAL_RGB01,
  MATERIALS,
  BOT_DYES,
  onScreen,
  REBUILD_MIN_MS,
  RECENT_GLOW_MS,
  pixelScale,
  sceneOf,
  scenePlayer,
  shotOf,
  shotCue,
  type ShotCue,
  skyOf,
  type Box,
  type McScene,
  type McShot,
  type ScenePlayer,
} from "../../core/index.ts";
import { McGl } from "./minecraft-gl.ts";
import type { McTiles, TileState } from "./minecraft-tiles.ts";

/** With reduced motion a tile is redrawn at most this often, and only when it changed. */
const STILL_REDRAW_MS = 250;
/** A landed copy keeps its source's camera this long, so the picture that flew in is the one that stays. */
const LANDED_CAMERA_MS = 6000;

export interface StageTile {
  id: string;
  slot: string;
  /** The tile's place in the wall: its camera's seed. */
  index: number;
  box: Box | null;
  /** The tile this one's world is a copy of. */
  forkOf: string | null;
  /** While the copy is being taken: the tile its picture flies from, and how long ago the fork was asked for. */
  flight: { from: string; elapsedMs: number } | null;
  /** The running timer of a tile that is restoring or taking a copy. */
  timer: string | null;
  /** Not running: restoring, taking a copy, or failed. */
  dark: boolean;
  /** This page's viewer holds the server: its bot is theirs to drive. */
  mine?: boolean;
}

/** A flight waits this long for the copy's world after the copy runs, then gives the tile back. */
const LANDING_WAIT_MS = 5000;

export interface StageFrame {
  /** The stage's box, as the tiles' boxes were laid out in it: the canvas covers it. */
  stage: Box;
  dpr: number;
  tiles: readonly StageTile[];
  reducedMotion: boolean;
  /** The tile's own canvas, for its names. */
  labels(id: string): HTMLCanvasElement | undefined;
  /** Where the flying pictures' frames go. */
  flights: HTMLElement | null;
}

interface Held {
  seq: number;
  key: string;
  recentSig: string;
  from: Uint8Array | null;
  fromSeq: number;
  scene: McScene;
  builtAt: number;
}

interface Flight {
  from: string;
  startedAt: number;
  /** When the copy stopped being taken (it runs now), while the flight waits for its world. */
  ranAt: number | null;
  landedAt: number | null;
  el: HTMLElement | null;
}

export interface StageDebug {
  /** Frames drawn since the page opened. */
  frames: number;
  level: number;
  /** Blocks a drawn column spans each way at this level. */
  step: number;
  still: boolean;
  /** Worlds holding buffers on the GPU. */
  held: number;
  calls: number;
  /** Per tile id: the shot it was last drawn from. */
  shots: Record<string, { kind: string; eye: number[]; follows: string | null }>;
  flights: string[];
  /** Median time from drawing one of the last frames to the animation frame that found the GPU done with it, ms. */
  frameMs: number | null;
  /** The browser draws WebGL on the CPU. */
  software: boolean;
  /** Median time the page's own code spent on one of them, ms. */
  scriptMs: number | null;
  /** The least time between drawn frames now, ms. */
  everyMs: number;
  /** Frames the GPU never reported done, drawn past after a wait. */
  stalls: number;
  /** Times a tile's geometry was built. */
  rebuilds: number;
  /** Times a tile's names were drawn. */
  nameDraws: number;
}

function botColor(p: ScenePlayer, names: readonly string[]): readonly [number, number, number] {
  if (!p.bot) return [1, 1, 1];
  const k = Math.max(0, names.indexOf(p.name));
  return MATERIAL_RGB01[MATERIALS.indexOf(BOT_DYES[k % BOT_DYES.length]!)]!;
}

export class McStage {
  private readonly gl: McGl;
  private readonly tiles: McTiles;
  private readonly budget: FrameBudget;
  /** Turned off by a test that looks at motion on a slow renderer. */
  budgetOn = true;
  private scenes = new Map<string, Held>();
  private flights = new Map<string, Flight>();
  /** Per tile: when a copy landed there and whose camera it keeps for a moment. */
  private landed = new Map<string, { at: number; seed: number }>();
  private shots = new Map<string, McShot>();
  /** Each tile's cue: the changed column its present cut features. */
  private cues = new Map<string, ShotCue>();
  /** A scene put its rebuild off (REBUILD_MIN_MS): a still view is not yet the picture of its world. */
  private owed = false;
  private lastDrawAt = Number.NEGATIVE_INFINITY;
  private lastDrew = false;
  private lastSig = "";
  private lastLayout = "";
  private costs: number[] = [];
  private script: number[] = [];
  private frames = 0;
  private rebuilds = 0;
  private nameDraws = 0;
  private everyMs = 33;
  /** What each tile's name canvas shows now: it is drawn again only when this changes. */
  private named = new WeakMap<HTMLCanvasElement, string>();
  /** The last animation frames' spacing: the display's tick. */
  private lastTick = Number.NaN;
  private ticks: number[] = [];

  /** Null when the browser has no WebGL2. `lost` is called if the context is lost later. */
  static create(canvas: HTMLCanvasElement, tiles: McTiles, preserve: boolean, lost: () => void): McStage | null {
    let stage: McStage | null = null;
    const gl = McGl.create(canvas, preserve, () => {
      // nothing more is drawn: a flight's frame must not stay where it was last put
      stage?.clearFlights();
      lost();
    });
    stage = gl === null ? null : new McStage(gl, tiles);
    return stage;
  }

  /** The view is drawn with less detail, or fewer frames, to keep up. */
  get reduced(): boolean {
    return this.budgetOn && this.budget.level > 0;
  }

  private clearFlights(): void {
    for (const fl of this.flights.values()) fl.el?.remove();
    this.flights.clear();
  }

  private constructor(gl: McGl, tiles: McTiles) {
    this.gl = gl;
    this.tiles = tiles;
    this.budget = new FrameBudget(gl.software);
  }

  /**
   * The tile's world as instances: rebuilt when the world, its fork's source, its glowing columns
   * or the detail changed, and at most every REBUILD_MIN_MS (a new launch or a new detail is
   * built at once).
   */
  private scene(t: StageTile, s: TileState, now: number, glow: boolean, step: number): McScene {
    const w = s.world!;
    for (const [i, at] of s.recent) if (now - at > RECENT_GLOW_MS) s.recent.delete(i);
    const recentSig = glow ? [...s.recent.keys()].join() : "";
    const from = t.forkOf === null ? null : this.tiles.source(t.id);
    const fromSeq = from?.seq ?? -1;
    const cur = this.scenes.get(t.id);
    if (cur && cur.key === s.key && cur.scene.step === step && cur.scene.w === w.region.w && cur.scene.h === w.region.h) {
      const same = cur.seq === w.seq && cur.recentSig === recentSig && cur.from === (from?.cells ?? null) && cur.fromSeq === fromSeq;
      if (same) return cur.scene;
      if (now - cur.builtAt < REBUILD_MIN_MS) {
        this.owed = true;
        return cur.scene;
      }
    }
    const scene = sceneOf(w, s.remap, { ground: w.ground ?? null, diffFrom: from?.cells ?? null, recent: glow && s.recent.size > 0 ? new Set(s.recent.keys()) : null, step });
    this.scenes.set(t.id, { seq: w.seq, key: s.key, recentSig, from: from?.cells ?? null, fromSeq, scene, builtAt: now });
    this.rebuilds++;
    return scene;
  }

  private drop(id: string): void {
    this.gl.release(id);
    this.scenes.delete(id);
    this.shots.delete(id);
    this.cues.delete(id);
  }

  /** One animation frame at `now` (local ms). Draws when its turn has come; returns whether it drew. */
  frame(now: number, f: StageFrame): boolean {
    if (this.gl.lost) return false;
    // The display's tick, from the spacing of the animation frames themselves.
    if (Number.isFinite(this.lastTick) && now > this.lastTick) {
      this.ticks.push(now - this.lastTick);
      if (this.ticks.length > 30) this.ticks.shift();
    }
    this.lastTick = now;
    // Before waiting for the GPU: a flight whose tiles left the stage ends now, not when the
    // last drawn frame is done.
    this.trackFlights(now, f);
    if (this.gl.busy(now)) return false;
    if (this.lastDrew) {
      // What the last drawn frame cost: the time until the GPU had drawn it, to the animation
      // frame that found it done.
      const cost = now - this.lastDrawAt;
      this.costs.push(cost);
      if (this.costs.length > 20) this.costs.shift();
      if (this.budgetOn) this.budget.sample(cost, now, displayTick(this.ticks));
      this.lastDrew = false;
    }
    const still = f.reducedMotion || (this.budgetOn && this.budget.still);
    // A fork in flight is drawn at least ten times a second, whatever the pace.
    const paced = Math.min(this.budgetOn ? this.budget.everyMs : 33, this.flights.size > 0 ? 100 : Number.POSITIVE_INFINITY);
    const every = f.reducedMotion ? Math.max(STILL_REDRAW_MS, paced) : paced;
    this.everyMs = every;
    const step = this.budgetOn ? this.budget.step : 1;
    // A tile that moved is drawn in its new place at once, whatever the pace.
    const layout = `${f.stage.x},${f.stage.y},${f.stage.w},${f.stage.h},${f.dpr}|${f.tiles.map((t) => (t.box ? `${t.id}:${t.box.x},${t.box.y},${t.box.w}` : "")).join()}`;
    if (layout === this.lastLayout && now - this.lastDrawAt < every - 4) return false;
    this.lastLayout = layout;
    if (still) {
      // A still view is drawn again only when something in it changed.
      const sig = `${layout}|${f.tiles.map((t) => `${this.tiles.peek(t.id)?.rev ?? -1}:${this.tiles.source(t.id)?.seq ?? -1}`).join()}|${this.flights.size}`;
      if (sig === this.lastSig && this.flights.size === 0 && !this.owed) return false;
      this.lastSig = sig;
    }
    this.owed = false;
    const gl = this.gl;
    gl.resize(f.stage.w, f.stage.h, pixelScale(f.stage.w, f.stage.h, f.dpr));
    gl.begin();
    const seen = new Set<string>();
    for (const t of f.tiles) {
      const s = this.tiles.peek(t.id);
      const canvas = f.labels(t.id);
      if (t.box === null || s === null || s.world === null || !onScreen(t.box, window.innerWidth, window.innerHeight)) {
        if (canvas) this.names(canvas, t.box, f.dpr, []);
        continue;
      }
      seen.add(t.id);
      const landed = this.landed.get(t.id);
      if (landed && now - landed.at > LANDED_CAMERA_MS) this.landed.delete(t.id);
      const shot = this.draw(t, s, now, t.box, f, still, this.landed.get(t.id)?.seed ?? t.index, step);
      if (canvas) {
        const players = s.players.map((p) => scenePlayer(s.world!.region, p));
        this.names(canvas, t.box, f.dpr, labelsOf(this.scenes.get(t.id)!.scene, players, shot.cam, t.box.w, t.box.h, shot.figScale));
      }
    }
    // What is no longer drawn gives its buffers back (a flight's picture goes with its flight, below).
    for (const id of [...this.scenes.keys()]) if (!seen.has(id) && !id.includes(">")) this.drop(id);
    this.drawFlights(now, f, still, step);
    gl.end(now);
    this.script.push(performance.timeOrigin + performance.now() - now);
    if (this.script.length > 20) this.script.shift();
    this.frames++;
    this.lastDrawAt = now;
    this.lastDrew = true;
    return true;
  }

  private draw(t: StageTile, s: TileState, now: number, box: Box, f: StageFrame, still: boolean, seed: number, step: number): McShot {
    const scene = this.scene(t, s, now, !f.reducedMotion, step);
    const players = s.players.map((p) => scenePlayer(s.world!.region, p));
    // The changed column this cut features is chosen when the cut begins and kept for it.
    const cue = still ? null : shotCue(this.cues.get(t.id) ?? null, { seed, timeMs: now, last: s.lastChange });
    if (cue !== null) this.cues.set(t.id, cue);
    const shot = shotOf({ scene, players, seed, timeMs: now, still, mine: t.mine === true, cue: cue?.site ?? null });
    const sky = skyOf(s.dayTime);
    const figures: number[] = [];
    for (const p of players) figureBoxes(figures, p, botColor(p, s.botNames), shot.figScale, { glow: sky.night, beacon: shot.figScale >= 1.25 });
    this.gl.world({
      id: t.id,
      rect: { x: box.x - f.stage.x, y: box.y - f.stage.y, w: box.w, h: box.h },
      scene,
      cam: shot.cam,
      sky,
      figures: new Float32Array(figures),
      lamps: players.slice(0, 3).map((p) => [p.x, p.y + 2.4, p.z]),
    });
    this.shots.set(t.id, shot);
    return shot;
  }

  /** The bots' names, on the tile's own canvas over the 3D picture: drawn again only when a name or its place changed. */
  private names(canvas: HTMLCanvasElement, box: Box | null, dpr: number, labels: readonly { name: string; x: number; y: number; driven: boolean }[]): void {
    const W = Math.max(1, Math.round((box?.w ?? 1) * dpr));
    const H = Math.max(1, Math.round((box?.h ?? 1) * dpr));
    const shown = box === null || box.w < 120 ? [] : labels;
    const sig = `${W}x${H}|${shown.map((l) => `${l.name}@${Math.round(l.x * dpr)},${Math.round(l.y * dpr)}${l.driven ? "!" : ""}`).join()}`;
    if (this.named.get(canvas) === sig && canvas.width === W && canvas.height === H) return;
    this.named.set(canvas, sig);
    this.nameDraws++;
    if (canvas.width !== W || canvas.height !== H) {
      canvas.width = W;
      canvas.height = H;
    }
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.clearRect(0, 0, W, H);
    if (shown.length === 0 || box === null) return;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.font = `600 ${box.w >= 480 ? 13 : 10}px "IBM Plex Mono", ui-monospace, monospace`;
    ctx.textAlign = "center";
    ctx.textBaseline = "alphabetic";
    ctx.lineJoin = "round";
    for (const l of shown) {
      ctx.lineWidth = 3;
      ctx.strokeStyle = "rgba(0, 0, 0, 0.8)";
      ctx.strokeText(l.name, l.x, l.y);
      ctx.fillStyle = l.driven ? "#7cc4ff" : "#ffffff";
      ctx.fillText(l.name, l.x, l.y);
    }
  }

  /**
   * A flight starts when the view says a copy's picture flies (the page can draw it), lands when
   * the copy runs and its world is in, and ends at once when it can no longer be drawn or the
   * copy failed: the tile then shows its own timer or error again.
   */
  private trackFlights(now: number, f: StageFrame): void {
    const live = new Map(f.tiles.map((t) => [t.id, t]));
    for (const t of f.tiles) {
      // The flight began with the fork's click, however late this page first draws it.
      if (t.flight !== null && !this.flights.has(t.id)) this.flights.set(t.id, { from: t.flight.from, startedAt: now - t.flight.elapsedMs, ranAt: null, landedAt: null, el: null });
    }
    for (const [id, fl] of this.flights) {
      const t = live.get(id);
      const from = live.get(fl.from);
      const done = fl.landedAt !== null && now - fl.landedAt >= FLIGHT_LAND_MS;
      // Still being taken: the view decides (it ends the flight when the source leaves the stage).
      const flying = t?.flight?.from === fl.from;
      // Taken: it runs as a copy of that source, both tiles are on the stage, and its world is awaited.
      const landing = !flying && t !== undefined && !t.dark && t.forkOf === fl.from && t.box !== null && from?.box != null && !f.reducedMotion;
      if (landing) fl.ranAt ??= now;
      if (done || !(flying || landing) || (fl.landedAt === null && fl.ranAt !== null && now - fl.ranAt > LANDING_WAIT_MS)) {
        fl.el?.remove();
        this.flights.delete(id);
        if (done && t) this.landed.set(id, { at: now, seed: from?.index ?? t.index });
        continue;
      }
      if (landing && fl.landedAt === null && this.tiles.peek(id)?.world != null) fl.landedAt = now;
    }
  }

  /** Each flight: the source's world drawn once more, in a box on its way to the tile taking the copy, with a frame that says so. */
  private drawFlights(now: number, f: StageFrame, still: boolean, step: number): void {
    const by = new Map(f.tiles.map((t) => [t.id, t]));
    for (const [id, fl] of this.flights) {
      const to = by.get(id);
      const from = by.get(fl.from);
      const s = this.tiles.peek(fl.from);
      if (!to?.box || !from?.box || !s || s.world === null) {
        // Nothing to draw this frame (the source's world is between sockets): no frame round nothing.
        if (fl.el) fl.el.style.display = "none";
        continue;
      }
      const rect = flightBox(from.box, to.box, flightProgress(now - fl.startedAt, fl.landedAt === null ? null : now - fl.landedAt));
      this.draw({ ...from, id: `${fl.from}>${id}`, forkOf: null }, s, now, rect, f, still, from.index, step);
      if (f.flights) {
        if (fl.el === null) {
          fl.el = document.createElement("div");
          fl.el.className = "mc-flight";
          fl.el.dataset["testid"] = `mc-flight-${to.slot}`;
          fl.el.append(document.createElement("span"), document.createElement("span"));
          f.flights.append(fl.el);
        }
        fl.el.style.display = "";
        fl.el.style.transform = `translate(${rect.x}px, ${rect.y}px)`;
        fl.el.style.width = `${rect.w}px`;
        fl.el.style.height = `${rect.h}px`;
        fl.el.children[0]!.textContent = `copy of ${from.slot}`;
        fl.el.children[1]!.textContent = to.timer ?? "";
      }
    }
    // The flying picture's buffers go with its flight.
    for (const id of [...this.scenes.keys()]) if (id.includes(">") && !this.flights.has(id.split(">")[1]!)) this.drop(id);
  }

  /** The tab is hidden: nothing stays on the GPU. Worlds are uploaded again when next drawn. */
  release(): void {
    this.gl.releaseAll();
    this.lastSig = "";
  }

  debug(): StageDebug {
    const sorted = [...this.costs].sort((a, b) => a - b);
    const script = [...this.script].sort((a, b) => a - b);
    return {
      frames: this.frames,
      level: this.budget.level,
      step: this.budgetOn ? this.budget.step : 1,
      still: this.budget.still,
      held: this.gl.held,
      calls: this.gl.calls,
      shots: Object.fromEntries([...this.shots].map(([id, s]) => [id, { kind: s.kind, eye: [...s.cam.eye], follows: s.follows }])),
      flights: [...this.flights.keys()],
      frameMs: sorted.length === 0 ? null : sorted[sorted.length >> 1]!,
      software: this.gl.software,
      scriptMs: script.length === 0 ? null : script[script.length >> 1]!,
      everyMs: this.everyMs,
      stalls: this.gl.stalls,
      rebuilds: this.rebuilds,
      nameDraws: this.nameDraws,
    };
  }

  dispose(): void {
    this.clearFlights();
    this.gl.dispose();
  }
}
