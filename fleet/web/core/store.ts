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
}

export interface UiState {
  focusId: string | null;
  hoverId: string | null;
  /** The tile the keyboard grid has focus on. */
  keyId: string | null;
  /** The pointer is over the image (or a long press is held). */
  pointerOverImage: boolean;
  stats: boolean;
  lightMode: boolean;
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
  | { type: "notice-expired"; before: number };

const HANDLED_KEPT = 512;
const NOTICES_KEPT = 6;
const WATCHES_KEPT = 32;

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
    ui: { focusId: null, hoverId: null, keyId: null, pointerOverImage: false, stats: false, lightMode: false },
  };
}

/** Pending intents whose receipt has been seen and whose version the page has accepted are done. */
function settle(pending: PendingIntent[], version: number): PendingIntent[] {
  return pending.filter((p) => p.receipt === null || p.receipt.version > version);
}

function latestKeyT(state: FleetState | null): number | null {
  if (!state || state.scene.camera.length === 0) return null;
  return Math.max(...state.scene.camera.map((k) => k.t));
}

/**
 * When a camera intent's move shows: the key the coordinator tagged with the
 * intent's id, or, untagged, the first key newer than any the page held when it
 * sent the intent, once the receipt's version is accepted.
 */
function resolveWatch(w: CameraWatch, state: FleetState | null, version: number): CameraWatch {
  if (w.tEff !== null || !state) return w;
  for (const k of state.scene.camera) if (k.intent === w.id) return { ...w, tEff: k.t };
  if (w.receiptVersion === null || version < w.receiptVersion) return w;
  const newer = state.scene.camera.filter((k) => w.keysBefore === null || k.t > w.keysBefore).map((k) => k.t);
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
        e.intent.name === "camera"
          ? [
              ...s.cameraWatches,
              { id: e.intent.id, inputAt: e.at, keysBefore: latestKeyT(s.accepted), receiptVersion: null, tEff: null },
            ].slice(-WATCHES_KEPT)
          : s.cameraWatches;
      return { ...s, pending, cameraWatches };
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
        case "snapshot": {
          if (msg.version <= s.version) return s; // older or repeated: dropped
          const accepted = msg.state;
          const cameraWatches = s.cameraWatches.map((w) => resolveWatch(w, accepted, msg.version));
          return { ...s, accepted, version: msg.version, pending: settle(s.pending, msg.version), cameraWatches };
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
                w.id === msg.id ? resolveWatch({ ...w, receiptVersion: msg.version }, s.accepted, s.version) : w,
              )
            : s.cameraWatches.filter((w) => w.id !== msg.id);
          return { ...s, handled, pending, notices, cameraWatches };
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
