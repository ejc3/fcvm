// The page's state and the one function that changes it. Every input (a socket
// message, a sent intent, a tick of the clock, a click) arrives as an event with
// its local time; the reducer has no clock of its own.

import {
  initialClock,
  pingSent,
  clockReceived,
  type ClockState,
} from "./clock.ts";
import {
  parseServerMsg,
  ProtocolError,
  receiptApplied,
  type CameraKey,
  type Claim,
  type FleetState,
  type IntentMsg,
  type IntentName,
  type ReceiptMsg,
} from "./protocol.ts";

export interface PendingIntent {
  intent: IntentMsg;
  sentAt: number;
  /** Set when the receipt arrives; the intent stays pending until a snapshot at its version does. */
  receipt: ReceiptMsg | null;
}

export interface Notice {
  id: string;
  kind: "refused" | "applied";
  intent: IntentName;
  reason: string | null;
  args: Record<string, unknown>;
  at: number;
}

export interface CameraWatch {
  id: string;
  inputAt: number;
  /** The latest camera key t the page held when it sent the intent. */
  keysBefore: number | null;
  /** The version its receipt named, once the receipt arrives. */
  receiptVersion: number | null;
  /** Scene t from which frames show the move, once known. */
  tEff: number | null;
}

export interface TileFrameStats {
  connected: boolean;
  fps: number;
  newestT: number | null;
  width: number;
  height: number;
  /** Shown dimmed: behind the common time. */
  behind: boolean;
  decodeMs: number | null;
}

export interface FrameStats {
  T: number | null;
  tiles: Record<string, TileFrameStats>;
  drawMsP50: number;
  drawMsP95: number;
  rafMsP50: number;
  rafMsP95: number;
  /** Every region the image draws has shown a frame (the live image is whole). */
  allLive: boolean;
}

export type StreamMode = "direct" | "aggregated";

export interface UiState {
  /** How this viewer receives the image: the mode it wants (aggregated unless it asks to see every VM). */
  mode: StreamMode;
  /** The mode the canvas is drawing now; it changes when the switch is complete. */
  shownMode: StreamMode;
  focusId: string | null;
  hoverId: string | null;
  /** The tile the keyboard grid has focus on. */
  keyId: string | null;
  /** The pointer is over the image (or a long press is held). */
  pointerOverImage: boolean;
  stats: boolean;
  lightMode: boolean;
  /** The Play drawer is open. */
  drawer: boolean;
  /** The viewer paused the moving picture. */
  paused: boolean;
  /** The wall tile whose GPU this viewer borrowed, while it comes back into the image. */
  returning: string | null;
  /** When the tab was hidden (local Unix ms), or null while it is visible. */
  hiddenSince: number | null;
  /** The frame streams are closed: the tab has been hidden for HIDDEN_PAUSE_MS. */
  suspended: boolean;
  /** The recording is on screen (from asleep until the live image has replaced it). */
  reelOn: boolean;
  /** How this viewer's next message is drawn. */
  textStyle: "lockup" | "skywriting" | "landmark";
}

/** A tab hidden this long stops its frame streams; the coordinator socket stays. */
export const HIDDEN_PAUSE_MS = 5 * 60_000;

/**
 * Whether the streams should be closed: hidden for at least the limit, judged
 * from the visibilitychange time, so a throttled background timer that fires
 * late still decides correctly.
 */
export function streamsSuspended(hiddenSince: number | null, now: number, limitMs: number = HIDDEN_PAUSE_MS): boolean {
  return hiddenSince !== null && now - hiddenSince >= limitMs;
}

/** A GPU reading kept for the detail view's history. */
export interface GpuSample {
  at: number;
  temp_c: number | null;
  power_w: number | null;
  util_pct: number | null;
}

/** An action held until this connection is verified. */
export interface QueuedAction {
  name: IntentName;
  args: Record<string, unknown>;
}

export interface TurnstileState {
  /** The site key from hello; null when actions need no verification. */
  sitekey: string | null;
  verified: boolean;
  /** A token was sent and its answer has not come. */
  verifying: boolean;
  error: string | null;
  /** Actions pressed before verification, sent once it succeeds. */
  queued: QueuedAction[];
}

export interface OwnGpu {
  /** The claim_gpu intent's id, while its receipt is awaited. */
  requestId: string | null;
  requestedAt: number | null;
  claim: Claim | null;
  /** Why the coordinator refused the claim. */
  refused: string | null;
}

/**
 * The tiling the canvas draws. A new layout generation is drawn only once every
 * tile of it has a frame at or after its `from_frame`; until the frame drawer says
 * so ("gen-shown"), the page lays its overlays out on the tiling still drawn.
 */
export interface ShownLayout {
  gen: number;
  tiles: FleetState["tiles"];
  aggregates: FleetState["aggregates"];
}

export interface ClientState {
  connection: "connecting" | "open" | "closed";
  accepted: FleetState | null;
  version: number;
  pending: PendingIntent[];
  /** Receipts already handled, so a repeated receipt does nothing. */
  handled: string[];
  notices: Notice[];
  clock: ClockState;
  frames: FrameStats | null;
  cameraWatches: CameraWatch[];
  lastInputLatencyMs: number | null;
  protocolErrors: number;
  lastProtocolError: string | null;
  /** The last message the coordinator sent as {type:"error"}. */
  serverError: string | null;
  /** This connection's viewer id, once the coordinator says it. */
  you: string | null;
  turnstile: TurnstileState;
  own: OwnGpu;
  /** Recent GPU readings per tile, oldest first. */
  gpuHistory: Record<string, GpuSample[]>;
  shownLayout: ShownLayout | null;
  ui: UiState;
}

export type ClientEvent =
  | { type: "socket"; status: "connecting" | "open" | "closed"; at: number }
  | { type: "message"; text: string; at: number }
  | { type: "intent-sent"; intent: IntentMsg; at: number }
  | { type: "ping-sent"; at: number }
  | { type: "frame-stats"; stats: FrameStats }
  | { type: "camera-drawn"; id: string; drawnAt: number }
  | { type: "ui"; patch: Partial<UiState> }
  | { type: "notice-expired"; before: number }
  /** An action pressed before verification. */
  | { type: "queue"; action: QueuedAction }
  | { type: "verify-sent" }
  /** The queued actions were sent. */
  | { type: "queue-sent" }
  | { type: "turnstile-error"; error: string }
  /** The viewer closed its own-GPU view, or dismissed a refusal. */
  | { type: "own-closed" }
  /** The frame drawer now draws this layout generation. */
  | { type: "gen-shown"; gen: number };

const HANDLED_KEPT = 512;
const NOTICES_KEPT = 6;
const WATCHES_KEPT = 32;
/** GPU readings kept per tile: two minutes at the coordinator's two-second cadence. */
export const GPU_HISTORY = 60;

export function initialState(): ClientState {
  return {
    connection: "connecting",
    accepted: null,
    version: -1,
    pending: [],
    handled: [],
    notices: [],
    clock: initialClock(),
    frames: null,
    cameraWatches: [],
    lastInputLatencyMs: null,
    protocolErrors: 0,
    lastProtocolError: null,
    serverError: null,
    you: null,
    turnstile: { sitekey: null, verified: false, verifying: false, error: null, queued: [] },
    own: { requestId: null, requestedAt: null, claim: null, refused: null },
    gpuHistory: {},
    shownLayout: null,
    ui: {
      mode: "aggregated",
      shownMode: "aggregated",
      focusId: null,
      hoverId: null,
      keyId: null,
      pointerOverImage: false,
      stats: false,
      lightMode: false,
      drawer: false,
      paused: false,
      returning: null,
      hiddenSince: null,
      suspended: false,
      reelOn: false,
      textStyle: "lockup",
    },
  };
}

/** Pending intents whose receipt has been seen and whose version the page has accepted are done. */
function settle(pending: PendingIntent[], version: number): PendingIntent[] {
  return pending.filter((p) => p.receipt === null || p.receipt.version > version);
}

/** The camera keys a viewer's own camera intents land in: its own GPU's. */
function ownKeys(s: ClientState): CameraKey[] | null {
  return s.own.claim?.camera ?? null;
}

function latestKeyT(keys: readonly CameraKey[] | null): number | null {
  if (!keys || keys.length === 0) return null;
  return Math.max(...keys.map((k) => k.t));
}

function appendGpu(history: Record<string, GpuSample[]>, st: FleetState, at: number): Record<string, GpuSample[]> {
  let out = history;
  for (const t of st.tiles) {
    if (!t.gpu) continue;
    const h = history[t.id] ?? [];
    const last = h[h.length - 1];
    const same = last && last.temp_c === t.gpu.temp_c && last.power_w === t.gpu.power_w && last.util_pct === t.gpu.util_pct;
    if (same) continue;
    if (out === history) out = { ...history };
    out[t.id] = [...h, { at, temp_c: t.gpu.temp_c, power_w: t.gpu.power_w, util_pct: t.gpu.util_pct }].slice(-GPU_HISTORY);
  }
  return out;
}

/**
 * When a camera intent's move shows: the key the coordinator tagged with the
 * intent's id, or, untagged, the first key newer than any the page held when it
 * sent the intent, once the receipt's version is accepted.
 */
function resolveWatch(w: CameraWatch, keys: readonly CameraKey[] | null, version: number): CameraWatch {
  if (w.tEff !== null || !keys) return w;
  for (const k of keys) if (k.intent === w.id) return { ...w, tEff: k.t };
  if (w.receiptVersion === null || version < w.receiptVersion) return w;
  const newer = keys.filter((k) => w.keysBefore === null || k.t > w.keysBefore).map((k) => k.t);
  return newer.length === 0 ? w : { ...w, tEff: Math.min(...newer) };
}

export function reduce(s: ClientState, e: ClientEvent): ClientState {
  switch (e.type) {
    case "socket":
      return {
        ...s,
        connection: e.status,
        clock: e.status === "open" ? s.clock : { ...s.clock, outstanding: null },
      };
    case "ping-sent":
      return { ...s, clock: pingSent(s.clock, e.at) };
    case "intent-sent": {
      if (s.pending.some((p) => p.intent.id === e.intent.id)) return s;
      const pending = [...s.pending, { intent: e.intent, sentAt: e.at, receipt: null }];
      const cameraWatches =
        e.intent.name === "claim_camera"
          ? [
              ...s.cameraWatches,
              { id: e.intent.id, inputAt: e.at, keysBefore: latestKeyT(ownKeys(s)), receiptVersion: null, tEff: null },
            ].slice(-WATCHES_KEPT)
          : s.cameraWatches;
      const own =
        e.intent.name === "claim_gpu" ? { requestId: e.intent.id, requestedAt: e.at, claim: null, refused: null } : s.own;
      return { ...s, pending, cameraWatches, own };
    }
    case "frame-stats":
      return { ...s, frames: e.stats };
    case "camera-drawn": {
      const w = s.cameraWatches.find((x) => x.id === e.id);
      if (!w) return s;
      return {
        ...s,
        cameraWatches: s.cameraWatches.filter((x) => x.id !== e.id),
        lastInputLatencyMs: Math.max(0, e.drawnAt - w.inputAt),
      };
    }
    case "ui":
      return { ...s, ui: { ...s.ui, ...e.patch } };
    case "notice-expired":
      return { ...s, notices: s.notices.filter((n) => n.at >= e.before) };
    case "queue":
      return { ...s, turnstile: { ...s.turnstile, queued: [...s.turnstile.queued, e.action].slice(-8), error: null } };
    case "verify-sent":
      return { ...s, turnstile: { ...s.turnstile, verifying: true, error: null } };
    case "queue-sent":
      return { ...s, turnstile: { ...s.turnstile, queued: [] } };
    case "turnstile-error":
      return { ...s, turnstile: { ...s.turnstile, verifying: false, error: e.error } };
    case "gen-shown": {
      const st = s.accepted;
      if (!st || st.layout.generation !== e.gen) return s;
      return { ...s, shownLayout: { gen: st.layout.generation, tiles: st.tiles, aggregates: st.aggregates } };
    }
    case "own-closed":
      return { ...s, own: { requestId: null, requestedAt: null, claim: null, refused: null } };
    case "message": {
      let msg;
      try {
        msg = parseServerMsg(e.text);
      } catch (err) {
        return {
          ...s,
          protocolErrors: s.protocolErrors + 1,
          lastProtocolError: err instanceof ProtocolError ? err.message : "unreadable message",
        };
      }
      switch (msg.type) {
        case "clock":
          return { ...s, clock: clockReceived(s.clock, msg.server_unix_ms, e.at, msg.client_unix_ms) };
        case "error":
          return { ...s, serverError: msg.reason };
        case "hello":
          // A new connection: verification belongs to the connection, so it starts over.
          return {
            ...s,
            you: msg.viewer,
            turnstile: { ...s.turnstile, sitekey: msg.turnstile, verified: false, verifying: false, error: null },
          };
        case "verified":
          return {
            ...s,
            turnstile: { ...s.turnstile, verified: msg.ok, verifying: false, error: msg.ok ? null : msg.reason ?? "verification failed" },
          };
        case "claim": {
          // Only the claim this page asked for, or the one it holds.
          if (s.own.requestId === null && s.own.claim === null) return s;
          if (s.own.claim !== null && s.own.claim.id !== msg.claim.id) return s;
          const own = { ...s.own, requestId: null, claim: msg.claim };
          const cameraWatches = s.cameraWatches.map((w) => resolveWatch(w, msg.claim.camera, s.version));
          return { ...s, own, cameraWatches };
        }
        case "snapshot": {
          if (msg.version <= s.version) return s; // older or repeated: dropped
          const accepted = msg.state;
          const cameraWatches = s.cameraWatches.map((w) => resolveWatch(w, ownKeys(s), msg.version));
          // The same generation (or the first): overlays follow it at once. A newer one
          // waits until the frame drawer has swapped to it.
          const shownLayout =
            s.shownLayout === null || accepted.layout.generation <= s.shownLayout.gen
              ? { gen: accepted.layout.generation, tiles: accepted.tiles, aggregates: accepted.aggregates }
              : s.shownLayout;
          return {
            ...s,
            accepted,
            shownLayout,
            version: msg.version,
            pending: settle(s.pending, msg.version),
            cameraWatches,
            gpuHistory: appendGpu(s.gpuHistory, accepted, e.at),
          };
        }
        case "receipt": {
          if (s.handled.includes(msg.id)) return s; // a repeated receipt
          const handled = [...s.handled, msg.id].slice(-HANDLED_KEPT);
          const mine = s.pending.find((p) => p.intent.id === msg.id);
          const applied = receiptApplied(msg);
          let pending = s.pending;
          let notices = s.notices;
          if (mine) {
            pending = applied
              ? settle(
                  s.pending.map((p) => (p.intent.id === msg.id ? { ...p, receipt: msg } : p)),
                  s.version,
                )
              : s.pending.filter((p) => p.intent.id !== msg.id);
            if (!applied || mine.intent.name === "word") {
              notices = [
                ...s.notices,
                {
                  id: msg.id,
                  kind: applied ? ("applied" as const) : ("refused" as const),
                  intent: mine.intent.name,
                  reason: msg.reason ?? null,
                  args: mine.intent.args,
                  at: e.at,
                },
              ].slice(-NOTICES_KEPT);
            }
          }
          const cameraWatches = applied
            ? s.cameraWatches.map((w) =>
                w.id === msg.id ? resolveWatch({ ...w, receiptVersion: msg.version }, ownKeys(s), s.version) : w,
              )
            : s.cameraWatches.filter((w) => w.id !== msg.id);
          let own = s.own;
          if (s.own.requestId === msg.id) {
            // Applied: the claim message (which can come before or after this) ends the request.
            if (!applied) own = { ...s.own, requestId: null, requestedAt: null, refused: msg.reason ?? "refused" };
          }
          return { ...s, handled, pending, notices, cameraWatches, own };
        }
      }
    }
  }
}

/** Intents to send again after the socket reopens: those with no receipt yet. */
export function intentsToResend(s: ClientState): IntentMsg[] {
  return s.pending.filter((p) => p.receipt === null).map((p) => p.intent);
}

/** Camera keys the frame drawer should watch for: drawn T at or past tEff ends the watch. */
export function cameraTargets(s: ClientState): { id: string; tEff: number }[] {
  return s.cameraWatches.flatMap((w) => (w.tEff === null ? [] : [{ id: w.id, tEff: w.tEff }]));
}

/** Build an intent. The id comes from the caller (randomness is an argument). */
export function makeIntent(id: string, name: IntentName, args: Record<string, unknown> = {}): IntentMsg {
  return { type: "intent", id, name, args };
}
