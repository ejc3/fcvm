// Platform glue for the page: the coordinator socket, the clock pings, the
// frame worker, and the store. Every rule lives in core/; this file only moves
// events in and view models out.

import {
  cameraTargets,
  initialState,
  intentsToResend,
  makeIntent,
  needsVerify,
  nextPingDelay,
  NOTICE_MS,
  reconnectDelayMs,
  reduce,
  shouldPing,
  type ClientEvent,
  type ClientState,
  type DrawPlan,
  type IntentName,
  type UiState,
} from "../../core/index.ts";
import type { BenchStats, CaptureResult, DrawSettings, FromWorker, ToWorker, TraceEntry } from "./worker-msgs.ts";

const now = () => performance.timeOrigin + performance.now();

function newId(): string {
  return crypto.randomUUID();
}

export class WallClient {
  private state: ClientState = initialState();
  private listeners = new Set<() => void>();
  private ws: WebSocket | null = null;
  private attempt = 0;
  private stopped = false;
  private pingTimer: ReturnType<typeof setTimeout> | null = null;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private noticeTimer: ReturnType<typeof setInterval> | null = null;
  private worker: Worker | null = null;
  private lastPlan = "";
  private lastWatch = "";
  private captureReq = 0;
  private captureWaiters = new Map<number, (r: CaptureResult) => void>();
  bench: BenchStats | null = null;
  /** Share of drawn tiles behind the common t, over the last ten seconds of direct mode. */
  lateShare: number | null = null;
  private lateSamples: number[] = [];
  private traceWaiters = new Map<number, (e: TraceEntry[]) => void>();
  /** The canvas has painted the poster or a frame. */
  painted = false;
  private paintedListeners = new Set<() => void>();
  readonly url: string;

  constructor(url: string) {
    this.url = url;
  }

  // ---- store ------------------------------------------------------------------

  getState = (): ClientState => this.state;

  subscribe = (fn: () => void): (() => void) => {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  };

  dispatch(e: ClientEvent): void {
    const before = this.state;
    const next = reduce(this.state, e);
    if (next === this.state) return;
    // Verified just now: send what the viewer pressed while it was being checked.
    if (!before.turnstile.verified && next.turnstile.verified && next.turnstile.queued.length > 0) {
      const queued = next.turnstile.queued;
      this.state = next;
      this.dispatch({ type: "queue-sent" });
      for (const q of queued) this.send(q.name, q.args);
      return;
    }
    const hadWatches = cameraTargets(this.state);
    this.state = next;
    const watches = cameraTargets(next);
    if (watches.length !== hadWatches.length || watches.some((w, i) => w.tEff !== hadWatches[i]?.tEff)) this.postWatch();
    for (const l of this.listeners) l();
    // A claim came for a request the viewer dismissed: give the GPU back.
    const release = next.own.release;
    if (release !== null && before.own.release !== release) {
      this.send("release_gpu", { claim: release });
      this.dispatch({ type: "own-released", claim: release });
    }
  }

  // ---- coordinator socket -----------------------------------------------------

  start(): void {
    this.stopped = false;
    this.connect();
    this.noticeTimer = setInterval(() => this.dispatch({ type: "notice-expired", before: now() - NOTICE_MS }), 1000);
  }

  stop(): void {
    this.stopped = true;
    if (this.pingTimer) clearTimeout(this.pingTimer);
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
    if (this.noticeTimer) clearInterval(this.noticeTimer);
    this.ws?.close();
    this.ws = null;
    this.worker?.terminate();
    this.worker = null;
  }

  /** Whether the tab is visible: hidden tabs do not count as viewers. */
  private visible = true;

  /** Tell the coordinator whether this tab is visible (also sent on every connect). */
  setVisible(visible: boolean): void {
    this.visible = visible;
    if (this.ws && this.ws.readyState === WebSocket.OPEN) this.ws.send(JSON.stringify({ type: "visibility", visible }));
  }

  private connect(): void {
    if (this.stopped) return;
    this.dispatch({ type: "socket", status: "connecting", at: now() });
    const ws = new WebSocket(this.url);
    this.ws = ws;
    ws.onopen = () => {
      this.attempt = 0;
      this.dispatch({ type: "socket", status: "open", at: now() });
      ws.send(JSON.stringify({ type: "visibility", visible: this.visible }));
      for (const intent of intentsToResend(this.state)) ws.send(JSON.stringify(intent));
      this.ping();
    };
    ws.onmessage = (e) => {
      if (typeof e.data === "string") this.dispatch({ type: "message", text: e.data, at: now() });
    };
    ws.onclose = () => {
      if (this.ws !== ws) return;
      this.ws = null;
      if (this.pingTimer) clearTimeout(this.pingTimer);
      this.dispatch({ type: "socket", status: "closed", at: now() });
      if (!this.stopped) this.reconnectTimer = setTimeout(() => this.connect(), reconnectDelayMs(this.attempt++));
    };
    ws.onerror = () => {};
  }

  private ping(): void {
    const ws = this.ws;
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    const at = now();
    if (shouldPing(this.state.clock, at)) {
      this.dispatch({ type: "ping-sent", at });
      ws.send(JSON.stringify({ type: "ping", client_unix_ms: at }));
    }
    this.pingTimer = setTimeout(() => this.ping(), nextPingDelay(this.state.clock));
  }

  /** Send an intent; returns its id. A given id is for tests that resend one. */
  send(name: IntentName, args: Record<string, unknown> = {}, id: string = newId()): string {
    const intent = makeIntent(id, name, args);
    this.dispatch({ type: "intent-sent", intent, at: now() });
    if (this.ws && this.ws.readyState === WebSocket.OPEN) this.ws.send(JSON.stringify(intent));
    return id;
  }

  /**
   * A viewer's action: sent now, or held until the Turnstile check passes when
   * the coordinator asks for one. Returns false when it was held.
   */
  act(name: IntentName, args: Record<string, unknown> = {}): boolean {
    if (needsVerify(this.state)) {
      this.dispatch({ type: "queue", action: { name, args } });
      return false;
    }
    this.send(name, args);
    return true;
  }

  /** A Turnstile token for this connection. */
  verify(token: string): void {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    this.dispatch({ type: "verify-sent" });
    this.ws.send(JSON.stringify({ type: "verify", token }));
  }

  onPainted(fn: () => void): () => void {
    if (this.painted) fn();
    this.paintedListeners.add(fn);
    return () => this.paintedListeners.delete(fn);
  }

  /** Send an intent message again, as a retry after a dropped socket would. */
  resend(id: string): void {
    const p = this.state.pending.find((x) => x.intent.id === id);
    const intent = p?.intent ?? null;
    if (intent && this.ws && this.ws.readyState === WebSocket.OPEN) this.ws.send(JSON.stringify(intent));
  }

  ui(patch: Partial<UiState>): void {
    this.dispatch({ type: "ui", patch });
  }

  // ---- frame worker -----------------------------------------------------------

  attachCanvas(canvas: HTMLCanvasElement, width: number, height: number, dpr: number): void {
    if (this.worker) return;
    const worker = new Worker(new URL("./frames.worker.ts", import.meta.url), { type: "module" });
    this.worker = worker;
    worker.onmessage = (e: MessageEvent<FromWorker>) => {
      const m = e.data;
      if (m.type === "stats") {
        this.bench = m.bench;
        if (this.state.ui.shownMode === "direct" && m.bench.tilesDrawn > 0) {
          this.lateSamples.push(m.bench.tilesBehind / m.bench.tilesDrawn);
          if (this.lateSamples.length > 20) this.lateSamples.shift();
          if (this.lateSamples.length >= 10) this.lateShare = this.lateSamples.reduce((a, b) => a + b, 0) / this.lateSamples.length;
        }
        this.dispatch({ type: "frame-stats", stats: m.stats });
      } else if (m.type === "camera-drawn") {
        this.dispatch({ type: "camera-drawn", id: m.id, drawnAt: m.drawnAt });
      } else if (m.type === "need-ticks") {
        const tick = () => {
          if (this.worker !== worker) return;
          worker.postMessage({ type: "tick" } satisfies ToWorker);
          requestAnimationFrame(tick);
        };
        requestAnimationFrame(tick);
      } else if (m.type === "gen-shown") {
        this.dispatch({ type: "gen-shown", gen: m.gen });
      } else if (m.type === "painted") {
        this.painted = true;
        for (const fn of this.paintedListeners) fn();
      } else if (m.type === "mode-shown") {
        this.dispatch({ type: "ui", patch: { shownMode: m.mode } });
      } else if (m.type === "trace") {
        this.traceWaiters.get(m.req)?.(m.entries);
        this.traceWaiters.delete(m.req);
      } else if (m.type === "capture") {
        this.captureWaiters.get(m.result.req)?.(m.result);
        this.captureWaiters.delete(m.result.req);
      }
    };
    const offscreen = canvas.transferControlToOffscreen();
    const msg: ToWorker = { type: "init", canvas: offscreen, width, height, dpr, mainTicks: false };
    worker.postMessage(msg, [offscreen]);
    this.lastPlan = "";
    this.postWatch();
  }

  resize(width: number, height: number, dpr: number): void {
    this.worker?.postMessage({ type: "resize", width, height, dpr } satisfies ToWorker);
  }

  plan(plan: DrawPlan, settings: DrawSettings): void {
    if (!this.worker) return;
    const key = JSON.stringify([plan, settings]);
    if (key === this.lastPlan) return;
    this.lastPlan = key;
    this.worker.postMessage({ type: "plan", plan, settings } satisfies ToWorker);
  }

  private postWatch(): void {
    if (!this.worker) return;
    const targets = cameraTargets(this.state);
    const key = JSON.stringify(targets);
    if (key === this.lastWatch) return;
    this.lastWatch = key;
    this.worker.postMessage({ type: "watch", targets } satisfies ToWorker);
  }

  /** Start recording draws (tests). */
  traceStart(): void {
    this.worker?.postMessage({ type: "trace", req: 0, on: true } satisfies ToWorker);
  }

  /** Stop recording draws and get them: t, mode and regions drawn without a frame (tests). */
  traceStop(): Promise<TraceEntry[]> {
    const req = ++this.captureReq;
    return new Promise((resolve) => {
      this.traceWaiters.set(req, resolve);
      this.worker?.postMessage({ type: "trace", req, on: false } satisfies ToWorker);
    });
  }

  /** Read back canvas pixels from the next draw (tests). Points are CSS pixels. */
  capture(points: { x: number; y: number }[]): Promise<CaptureResult> {
    const req = ++this.captureReq;
    return new Promise((resolve) => {
      this.captureWaiters.set(req, resolve);
      this.worker?.postMessage({ type: "capture", req, points } satisfies ToWorker);
    });
  }
}

/** Whether this browser can play the wall: WebCodecs H.264 and an offscreen canvas. */
export async function canDecodeH264(): Promise<boolean> {
  if (typeof VideoDecoder === "undefined" || typeof HTMLCanvasElement === "undefined") return false;
  if (!("transferControlToOffscreen" in HTMLCanvasElement.prototype)) return false;
  try {
    const s = await VideoDecoder.isConfigSupported({ codec: "avc1.42E01E", codedWidth: 640, codedHeight: 360 });
    return s.supported === true;
  } catch {
    return false;
  }
}
