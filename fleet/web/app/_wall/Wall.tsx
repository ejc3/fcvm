"use client";

// The wall page: the render is the page. The image fills the screen (contain-fit,
// centred); a row of controls sits over its top and a strip over its bottom, both on
// scrims, and the controls fade when the pointer rests. Everything else is in the
// drawer. The page measures the screen, forwards input as actions, and draws the
// view model core/ computes. It computes nothing itself.

import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";
import {
  CLICK_SLOP_PX,
  parseReelMeta,
  type ReelMeta,
  HIDDEN_PAUSE_MS,
  streamsSuspended,
  LONG_PRESS_MS,
  neighbour,
  posterUrl,
  tileAt,
  ZOOM_MS,
  ZOOM_SHEET_W,
  zoomInsetsFor,
  readingOrder,
  screenToImage,
  pointerTile,
  lightPosition,
  sceneT,
  serverNow,
  HUD_BAND,
  LIGHT_COLORS,
  wallView,
  type Box,
  type Direction,
  type WallView,
  type ZoomStep,
} from "../../core/index.ts";
import { canDecodeH264, WallClient } from "./wall-client.ts";
import { FocusPanel } from "./FocusPanel.tsx";
import { OwnGpu } from "./OwnGpu.tsx";
import { HalfClockView, Overlays, WakeCard } from "./Overlays.tsx";
import { PlayDrawer, VerifyBox, type DrawerPanel } from "./PlayDrawer.tsx";
import { Reel } from "./Reel.tsx";
import type { DrawSettings } from "./worker-msgs.ts";

const localNow = () => performance.timeOrigin + performance.now();
const MODE_KEY = "fleet-wall.mode";
const HINT_KEY = "fleet-wall.hint-seen";
/** The controls fade after this long with no pointer, key or touch. */
const IDLE_MS = 3000;
/** How long the one-time hint stays. */
const HINT_MS = 4000;
/** Pointer moves are passed on at most this often (they only refresh the seams' hold). */
const MOVE_EVERY_MS = 250;

/** A bandwidth budget from ?budget=<Mbit/s>, for trying the q=lo rule on a fast link. */
function budgetParam(): number | undefined {
  if (typeof window === "undefined") return undefined;
  const v = Number(new URLSearchParams(window.location.search).get("budget"));
  return Number.isFinite(v) && v > 0 ? v * 1e6 : undefined;
}

/** The hidden-tab limit; tests shorten it with ?debug=1&hidden_ms=<ms>. */
function hiddenLimitMs(): number {
  if (typeof window === "undefined") return HIDDEN_PAUSE_MS;
  const q = new URLSearchParams(window.location.search);
  const v = Number(q.get("hidden_ms"));
  return q.get("debug") === "1" && Number.isFinite(v) && v > 0 ? v : HIDDEN_PAUSE_MS;
}

/** The browser asks to save data (Chrome's Data Saver, a low-data mode). */
function saveData(): boolean {
  if (typeof navigator === "undefined") return false;
  const c = (navigator as unknown as { connection?: { saveData?: unknown } }).connection;
  return c?.saveData === true;
}

/** The browser's downlink estimate in Mbit/s, where it gives one (Chrome). */
function downlink(): number | null {
  if (typeof navigator === "undefined") return null;
  const c = (navigator as unknown as { connection?: { downlink?: unknown } }).connection;
  return typeof c?.downlink === "number" && c.downlink > 0 ? c.downlink : null;
}

function boxOf(el: Element | null): Box | null {
  if (!el) return null;
  const r = el.getBoundingClientRect();
  if (r.width <= 0 || r.height <= 0) return null;
  return { x: r.left, y: r.top, w: r.width, h: r.height };
}

function sameBox(a: Box | null, b: Box | null): boolean {
  if (a === null || b === null) return a === b;
  return a.x === b.x && a.y === b.y && a.w === b.w && a.h === b.h;
}

function useMedia(query: string): boolean {
  const subscribe = useCallback(
    (fn: () => void) => {
      const m = window.matchMedia(query);
      m.addEventListener("change", fn);
      return () => m.removeEventListener("change", fn);
    },
    [query],
  );
  return useSyncExternalStore(subscribe, () => window.matchMedia(query).matches, () => false);
}

declare global {
  interface Window {
    __fleet?: {
      client: WallClient;
      view: () => WallView;
    };
  }
}

type Areas = { mosaic: Box | null; own: Box | null };

/**
 * The zoom between the wall and an opened tile, stepped once per display frame for ZOOM_MS.
 * A change of target while one runs back the way it came reverses from where it is; any other
 * change starts from the last target. Null once it has zoomed back out to the wall.
 */
function useZoomStep(target: string | null, reducedMotion: boolean): ZoomStep | null {
  const [anim, setAnim] = useState<{ from: string | null; to: string | null; start: number }>({ from: null, to: null, start: 0 });
  const [progress, setProgress] = useState(1);
  if (target !== anim.to) {
    // Derived during render, so no frame shows the new target at the old step.
    const at = performance.now();
    const back = target === anim.from && progress < 1;
    setAnim({ from: anim.to, to: target, start: back ? at - (1 - progress) * ZOOM_MS : at });
    setProgress(reducedMotion ? 1 : back ? 1 - progress : 0);
  }
  useEffect(() => {
    if (reducedMotion) {
      setProgress(1);
      return;
    }
    let id = 0;
    const tick = () => {
      const p = Math.min(1, (performance.now() - anim.start) / ZOOM_MS);
      setProgress(p);
      if (p < 1) id = requestAnimationFrame(tick);
    };
    id = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(id);
  }, [anim, reducedMotion]);
  return useMemo(() => {
    if (anim.from === null && anim.to === null) return null;
    if (anim.to === null && progress >= 1) return null;
    return { from: anim.from, to: anim.to, progress };
  }, [anim, progress]);
}

function PauseIcon({ paused }: { paused: boolean }) {
  return (
    <svg width="18" height="18" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
      {paused ? <path d="M7 4.5v15l13-7.5z" /> : <path d="M6 4h4v16H6zM14 4h4v16h-4z" />}
    </svg>
  );
}

function InfoIcon() {
  return (
    <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" aria-hidden="true">
      <circle cx="12" cy="12" r="9" />
      <path d="M12 11v6M12 7.5v.5" />
    </svg>
  );
}

function MenuIcon() {
  return (
    <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" aria-hidden="true">
      <path d="M4 7h16M4 12h16M4 17h16" />
    </svg>
  );
}

const GITHUB_LABEL = "fcvm on GitHub";

function GithubIcon() {
  return (
    <svg width="20" height="20" viewBox="0 0 16 16" fill="currentColor" aria-hidden="true">
      <path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82a7.6 7.6 0 0 1 2-.27c.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.01 8.01 0 0 0 16 8c0-4.42-3.58-8-8-8z" />
    </svg>
  );
}

/** The repository link; the address is the one the isolation panel links to. */
function GithubLink({ href, label }: { href: string; label: string }) {
  return (
    <a className="icon-btn" href={href} target="_blank" rel="noopener noreferrer" aria-label={label} title={label} data-testid="github-link">
      <GithubIcon />
    </a>
  );
}

function RestartIcon() {
  return (
    <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M3 12a9 9 0 0 1 15.3-6.4L21 8" />
      <path d="M21 3v5h-5" />
      <path d="M21 12a9 9 0 0 1-15.3 6.4L3 16" />
      <path d="M3 21v-5h5" />
    </svg>
  );
}

export function Wall({ coordUrl, mediaUrl }: { coordUrl: string; mediaUrl: string }) {
  const client = useMemo(() => new WallClient(coordUrl), [coordUrl]);
  const poster = useMemo(() => posterUrl(coordUrl), [coordUrl]);
  // A coordinator with no poster yet (just restarted) answers 404: show no image rather than
  // the browser's broken-image icon.
  const [posterFailed, setPosterFailed] = useState(false);
  // The server-rendered <img> can fail before React attaches onError, so also check it once
  // mounted: a finished load with no picture is a failure.
  const posterRef = useRef<HTMLImageElement>(null);
  useEffect(() => {
    const el = posterRef.current;
    if (el && el.complete && el.naturalWidth === 0) setPosterFailed(true);
  }, [poster]);
  const state = useSyncExternalStore(client.subscribe, client.getState, client.getState);
  const [now, setNow] = useState(0);
  const [canDecode, setCanDecode] = useState(true);
  const [painted, setPainted] = useState(false);
  // The recording's metadata: null until read, or when it is missing.
  const [reelMeta, setReelMeta] = useState<ReelMeta | null>(null);
  const [dpr, setDpr] = useState(1);
  const [viewport, setViewport] = useState({ w: 0, h: 0 });
  const [areas, setAreas] = useState<Areas>({ mosaic: null, own: null });
  const [avoid, setAvoid] = useState<Box[]>([]);
  const [labelSize, setLabelSize] = useState<{ w: number; h: number } | undefined>(undefined);
  /** Which panel the drawer shows: the controls, About, or how a square is isolated. */
  const [drawerPanel, setDrawerPanel] = useState<DrawerPanel>("play");
  /** The word being typed; it outlives panel switches. */
  const [wordDraft, setWordDraft] = useState("");
  /** The button that opened the drawer; closing it returns focus there. */
  const drawerOpener = useRef<HTMLElement | null>(null);
  const phone = useMedia("(max-width: 700px)");
  const reducedMotion = useMedia("(prefers-reduced-motion: reduce)");
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const stageRef = useRef<HTMLDivElement | null>(null);
  const ownShotRef = useRef<HTMLDivElement | null>(null);
  const viewRef = useRef<WallView | null>(null);
  const gridRefs = useRef(new Map<string, HTMLButtonElement>());
  const returnFocus = useRef<HTMLElement | null>(null);
  const playButtonRef = useRef<HTMLButtonElement | null>(null);
  const lastMoveSent = useRef(0);
  const [idle, setIdle] = useState(false);
  const [hint, setHint] = useState<"unseen" | "showing" | "seen">("seen");

  // The controls fade when nothing moves for IDLE_MS, and come back on any pointer move,
  // key or touch; never while one of them has focus.
  useEffect(() => {
    let timer: ReturnType<typeof setTimeout> | null = null;
    const wake = () => {
      setIdle(false);
      if (timer) clearTimeout(timer);
      timer = setTimeout(() => {
        if (document.activeElement?.closest("[data-hud]")) return;
        setIdle(true);
      }, IDLE_MS);
    };
    wake();
    const events = ["pointermove", "pointerdown", "keydown", "wheel", "touchstart", "focusin"] as const;
    for (const e of events) window.addEventListener(e, wake, { passive: true });
    return () => {
      if (timer) clearTimeout(timer);
      for (const e of events) window.removeEventListener(e, wake);
    };
  }, []);

  // "Click a square" is said once per browser, the first time the pointer is over the image.
  useEffect(() => {
    try {
      if (window.localStorage.getItem(HINT_KEY) === null) setHint("unseen");
    } catch {
      setHint("unseen");
    }
  }, []);
  const overImage = state.ui.pointerOverImage;
  useEffect(() => {
    if (hint !== "unseen" || !overImage) return;
    setHint("showing");
    try {
      window.localStorage.setItem(HINT_KEY, "1");
    } catch {
      // Not remembered: it shows once more on the next visit.
    }
  }, [hint, overImage]);
  useEffect(() => {
    if (hint !== "showing") return;
    const id = setTimeout(() => setHint("seen"), HINT_MS);
    return () => clearTimeout(id);
  }, [hint]);

  // Clock, capability, connection.
  useEffect(() => {
    setNow(localNow());
    const id = setInterval(() => setNow(localNow()), 100);
    client.start();
    void canDecodeH264().then(setCanDecode);
    const off = client.onPainted(() => setPainted(true));
    return () => {
      clearInterval(id);
      off();
      client.stop();
    };
  }, [client]);

  // The recording's metadata, read once: its date and numbers are data, not text in the page.
  useEffect(() => {
    let gone = false;
    fetch(`${mediaUrl.replace(/\/$/, "")}/wall-reel.json`)
      .then((r) => (r.ok ? r.json() : null))
      .then((j: unknown) => {
        if (!gone) setReelMeta(parseReelMeta(j));
      })
      .catch(() => {
        // No metadata: the recording plays without its badge.
      });
    return () => {
      gone = true;
    };
  }, [mediaUrl]);

  // Tell the coordinator whether this tab is visible: hidden tabs do not count as viewers.
  useEffect(() => {
    const send = () => client.setVisible(document.visibilityState === "visible");
    send();
    document.addEventListener("visibilitychange", send);
    return () => document.removeEventListener("visibilitychange", send);
  }, [client]);

  // A tab hidden for HIDDEN_PAUSE_MS closes its frame streams; the coordinator
  // socket stays. The decision reads the visibilitychange time, so a background
  // timer the browser throttles still decides right when it fires.
  useEffect(() => {
    const limit = hiddenLimitMs();
    let timer: ReturnType<typeof setInterval> | null = null;
    const check = () => {
      const ui = client.getState().ui;
      const sus = streamsSuspended(ui.hiddenSince, localNow(), limit);
      if (sus !== ui.suspended) client.ui({ suspended: sus });
    };
    const onVisibility = () => {
      if (document.visibilityState === "hidden") {
        if (client.getState().ui.hiddenSince === null) client.ui({ hiddenSince: localNow() });
        timer ??= setInterval(check, Math.min(10_000, Math.max(250, limit / 4)));
      } else {
        if (timer) clearInterval(timer);
        timer = null;
        client.ui({ hiddenSince: null, suspended: false });
      }
    };
    document.addEventListener("visibilitychange", onVisibility);
    if (document.visibilityState === "hidden") onVisibility();
    return () => {
      document.removeEventListener("visibilitychange", onVisibility);
      if (timer) clearInterval(timer);
    };
  }, [client]);

  const ownOpen = state.own.requestId !== null || state.own.claim !== null;

  // Screen size and the measured areas.
  useEffect(() => {
    const measure = () => {
      setDpr(window.devicePixelRatio || 1);
      setViewport((v) => (v.w === window.innerWidth && v.h === window.innerHeight ? v : { w: window.innerWidth, h: window.innerHeight }));
      setAreas((a) => {
        const next = { mosaic: boxOf(stageRef.current), own: boxOf(ownShotRef.current) };
        return sameBox(a.mosaic, next.mosaic) && sameBox(a.own, next.own) ? a : next;
      });
    };
    measure();
    const ro = new ResizeObserver(measure);
    for (const el of [stageRef.current, ownShotRef.current]) if (el) ro.observe(el);
    window.addEventListener("resize", measure);
    const mq = window.matchMedia(`(resolution: ${window.devicePixelRatio}dppx)`);
    mq.addEventListener("change", measure);
    return () => {
      ro.disconnect();
      window.removeEventListener("resize", measure);
      mq.removeEventListener("change", measure);
    };
  }, [phone, state.ui.focusId, ownOpen, state.ui.drawer, state.accepted === null]);

  const zoom = useZoomStep(state.ui.focusId, reducedMotion);
  const zoomArea = useMemo(() => (viewport.w > 0 ? { x: 0, y: 0, w: viewport.w, h: viewport.h } : null), [viewport.w, viewport.h]);
  const zoomInsets = useMemo(() => zoomInsetsFor(phone, phone ? 0 : ZOOM_SHEET_W + 16), [phone]);
  // The sheet with an opened tile's details starts open, beside it on a wide screen and under it on a
  // phone, until the viewer folds it.
  const [sheetChoice, setSheetChoice] = useState<boolean | null>(null);
  const sheetOpen = sheetChoice ?? true;
  const view = useMemo(
    () =>
      wallView({
        state,
        now: now || 0,
        dpr,
        mosaicArea: areas.mosaic,
        zoom,
        zoomArea,
        zoomInsets,
        ownArea: ownOpen ? areas.own : null,
        canDecode,
        phone,
        reducedMotion,
        downlinkMbps: downlink(),
        lateShare: client.lateShare,
        avoid,
        labelSize,
        budgetBps: budgetParam(),
        poster,
        media: { base: mediaUrl, meta: reelMeta },
        saveData: saveData(),
      }),
    [state, now, dpr, areas, zoom, zoomArea, zoomInsets, canDecode, phone, reducedMotion, client, avoid, labelSize, poster, ownOpen, mediaUrl, reelMeta],
  );
  viewRef.current = view;

  // The recording stays on screen from the moment the fleet sleeps until the live image has replaced it.
  const sleeping = view.sleep !== null;
  useEffect(() => {
    if (sleeping && !client.getState().ui.reelOn) client.ui({ reelOn: true });
  }, [client, sleeping]);
  const reelDone = useCallback(() => client.ui({ reelOn: false }), [client]);

  // After each draw of the page, measure what the next view model places around.
  useLayoutEffect(() => {
    const boxes = [...document.querySelectorAll("[data-avoid]")].map((el) => boxOf(el)).filter((b): b is Box => b !== null);
    if (JSON.stringify(boxes) !== JSON.stringify(avoid)) setAvoid(boxes);
    const label = boxOf(document.querySelector(".hover-label"));
    if (label && (!labelSize || Math.abs(label.w - labelSize.w) > 1 || Math.abs(label.h - labelSize.h) > 1)) setLabelSize({ w: label.w, h: label.h });
  });

  // The viewer's choice of how streams arrive is remembered on this browser.
  useEffect(() => {
    try {
      const saved = window.localStorage.getItem(MODE_KEY);
      if (saved === "direct" || saved === "aggregated") client.ui({ mode: saved });
    } catch {
      // No storage here: the default stands.
    }
  }, [client]);
  const chooseMode = useRef(false);
  useEffect(() => {
    if (!chooseMode.current) {
      chooseMode.current = true;
      return;
    }
    try {
      window.localStorage.setItem(MODE_KEY, state.ui.mode);
    } catch {
      // Not remembered; nothing else depends on it.
    }
  }, [state.ui.mode]);

  // The frame worker gets the canvas once, then plans.
  useEffect(() => {
    const c = canvasRef.current;
    if (!c || !canDecode || viewport.w === 0) return;
    client.attachCanvas(c, viewport.w, viewport.h, dpr);
  }, [client, canDecode, viewport.w, viewport.h, dpr]);
  useEffect(() => {
    client.resize(viewport.w, viewport.h, dpr);
  }, [client, viewport.w, viewport.h, dpr]);
  const covered = view.focus !== null || view.own !== null || view.zoom !== null;
  const ownShown = view.own !== null;
  useEffect(() => {
    const settings: DrawSettings = { background: ownShown ? "#0B0D10" : "#000000", dimAlpha: 0.5, fadeInMs: 280 };
    client.plan(view.plan, settings);
  }, [client, view.plan, ownShown]);

  // Test hooks, only with ?debug=1.
  useEffect(() => {
    if (new URLSearchParams(window.location.search).get("debug") === "1") {
      window.__fleet = { client, view: () => viewRef.current! };
    }
  }, [client]);

  // ---- pointer input on the image ----------------------------------------------

  const press = useRef<{ id: number; x: number; y: number; moved: boolean; longPress: ReturnType<typeof setTimeout> | null; tile: string | null; long: boolean } | null>(null);

  const imagePoint = (x: number, y: number) => {
    const L = viewRef.current?.layout;
    return L ? screenToImage(L.image, x, y) : null;
  };
  const tileUnder = (x: number, y: number) => {
    const p = imagePoint(x, y);
    return p ? pointerTile(client.getState(), canDecode, p.u, p.v) : null;
  };

  const onPointerDown = (e: React.PointerEvent) => {
    if (covered || e.button !== 0) return;
    const tile = tileUnder(e.clientX, e.clientY);
    press.current = {
      id: e.pointerId,
      x: e.clientX,
      y: e.clientY,
      moved: false,
      tile: tile?.id ?? null,
      long: false,
      longPress:
        e.pointerType === "touch" && tile
          ? setTimeout(() => {
              client.ui({ hoverId: tile.id, pointerOverImage: true, longPress: true });
              if (press.current) {
                press.current.longPress = null;
                press.current.long = true;
              }
            }, LONG_PRESS_MS)
          : null,
    };
  };

  const onPointerMove = (e: React.PointerEvent) => {
    const d = press.current;
    if (d && d.id === e.pointerId) {
      if (!d.moved && Math.hypot(e.clientX - d.x, e.clientY - d.y) > CLICK_SLOP_PX && !d.long) {
        d.moved = true;
        if (d.longPress) clearTimeout(d.longPress);
        d.longPress = null;
      }
      if (e.pointerType === "touch") return;
    }
    if (e.pointerType === "touch") return;
    const tile = tileUnder(e.clientX, e.clientY);
    const over = imagePoint(e.clientX, e.clientY) !== null;
    const s = client.getState().ui;
    const at = localNow();
    const refresh = over && at - lastMoveSent.current >= MOVE_EVERY_MS;
    if (s.hoverId !== (tile?.id ?? null) || s.pointerOverImage !== over || refresh) {
      if (over) lastMoveSent.current = at;
      client.ui({ hoverId: tile?.id ?? null, pointerOverImage: over, ...(over ? { pointerMovedAt: at } : {}) });
    }
  };

  const endPointer = (e: React.PointerEvent, cancelled: boolean) => {
    const d = press.current;
    if (!d || d.id !== e.pointerId) return;
    if (d.longPress) clearTimeout(d.longPress);
    press.current = null;
    if (d.long) {
      client.ui({ hoverId: null, pointerOverImage: false, longPress: false });
      return;
    }
    if (d.moved || cancelled) return;
    const s = client.getState();
    if (s.ui.lightMode) {
      const p = imagePoint(e.clientX, e.clientY);
      const st = s.accepted;
      if (p && st) {
        // The world point under the click, on the camera as it was drawn.
        const t = s.frames?.T ?? sceneT(st.scene.epoch_unix_ms, serverNow(s.clock, localNow()));
        const pos = lightPosition(st.scene.camera, t, p.u, p.v, st.image_aspect);
        const color = LIGHT_COLORS[st.scene.lights.length % LIGHT_COLORS.length]!;
        if (pos) client.act("light", { pos, color });
      }
      client.ui({ lightMode: false });
      return;
    }
    const tile = tileUnder(e.clientX, e.clientY);
    if (tile) openTile(tile.id, null);
  };

  const onPointerLeave = (e: React.PointerEvent) => {
    if (e.pointerType === "touch" || press.current) return;
    client.ui({ hoverId: null, pointerOverImage: false });
  };

  // ---- open and close ----------------------------------------------------------------

  const openTile = (id: string, from: HTMLElement | null) => {
    returnFocus.current = from ?? (document.activeElement as HTMLElement | null);
    client.ui({ focusId: id, hoverId: null, pointerOverImage: false, drawer: false });
  };
  const restoreFocus = () => {
    const el = returnFocus.current;
    returnFocus.current = null;
    requestAnimationFrame(() => el?.focus());
  };
  // Focus goes back to where the tile was opened from once the zoom has run back out: the wall is hidden until then.
  const [restoreAfterZoom, setRestoreAfterZoom] = useState(false);
  const closeTile = useCallback(() => {
    client.ui({ focusId: null });
    setRestoreAfterZoom(true);
  }, [client]);
  const zoomGone = view.zoom === null;
  useEffect(() => {
    if (!restoreAfterZoom || !zoomGone) return;
    setRestoreAfterZoom(false);
    restoreFocus();
  }, [restoreAfterZoom, zoomGone]);
  // Opened, a click on a dimmed tile around it, or an arrow key, opens that tile instead.
  const pickZoomed = useCallback(
    (x: number, y: number) => {
      const v = viewRef.current;
      const st = client.getState();
      const L = v?.zoom?.layout;
      if (!L || !st.accepted) return;
      const p = screenToImage(L.image, x, y);
      const t = p ? tileAt(st.accepted.tiles, p.u, p.v) : null;
      if (t && t.id !== st.ui.focusId) client.ui({ focusId: t.id });
    },
    [client],
  );
  const moveZoomed = useCallback(
    (dir: Direction) => {
      const st = client.getState();
      const tiles = st.accepted?.tiles ?? [];
      const from = tiles.find((t) => t.id === st.ui.focusId);
      const to = from ? neighbour(tiles, from, dir) : null;
      if (to) client.ui({ focusId: to.id });
    },
    [client],
  );
  const closeDrawer = useCallback(() => {
    client.ui({ drawer: false });
    const opener = drawerOpener.current;
    requestAnimationFrame(() => (opener?.isConnected ? opener : playButtonRef.current)?.focus());
  }, [client]);
  const getOwnGpu = (from: HTMLElement) => {
    returnFocus.current = from;
    client.ui({ drawer: false, focusId: null });
    client.act("claim_gpu");
  };
  const closeOwn = useCallback(() => {
    const c = client.getState().own.claim;
    if (c && c.phase !== "ended") client.send("release_gpu", { claim: c.id });
    // Back on the wall, the viewer watches the tile whose GPU they borrowed come back.
    client.ui({ returning: c?.tile ?? null });
    client.dispatch({ type: "own-closed" });
    restoreFocus();
  }, [client]);
  // When the minute is up, the private view says so briefly, then returns to the wall on its own.
  const ownEnded = state.own.claim?.phase === "ended";
  useEffect(() => {
    if (!ownEnded) return;
    const id = setTimeout(closeOwn, 1800);
    return () => clearTimeout(id);
  }, [ownEnded, closeOwn]);
  const returningShown = view.returning;
  useEffect(() => {
    if (state.ui.returning !== null && state.accepted !== null && !returningShown) client.ui({ returning: null });
  }, [client, state.ui.returning, state.accepted, returningShown]);

  // ---- keyboard grid -------------------------------------------------------------------

  const order = useMemo(() => readingOrder(state.accepted?.tiles ?? []), [state.accepted]);
  const keyTarget = state.ui.keyId ?? order[0]?.id ?? null;
  const onGridKey = (e: React.KeyboardEvent, id: string) => {
    const dirs: Record<string, Direction> = { ArrowLeft: "left", ArrowRight: "right", ArrowUp: "up", ArrowDown: "down" };
    const dir = dirs[e.key];
    if (!dir) return;
    e.preventDefault();
    const tiles = state.accepted?.tiles ?? [];
    const from = tiles.find((t) => t.id === id);
    const to = from ? neighbour(tiles, from, dir) : null;
    if (to) gridRefs.current.get(to.id)?.focus();
  };

  const score = view.score;
  const stage = (
    <div
      ref={stageRef}
      className={`stage${state.ui.lightMode ? " light-mode" : ""}`}
      data-testid="stage"
      data-version={view.version}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={(e) => endPointer(e, false)}
      onPointerCancel={(e) => endPointer(e, true)}
      onPointerLeave={onPointerLeave}
      onContextMenu={(e) => e.preventDefault()}
    >
      {poster && !posterFailed ? (
        <img
          className={`poster${painted ? " gone" : ""}`}
          ref={posterRef}
          src={poster}
          onError={() => setPosterFailed(true)}
          alt=""
          aria-hidden="true"
          data-testid="poster"
          style={
            view.plan.image && areas.mosaic
              ? { left: view.plan.image.x - areas.mosaic.x, top: view.plan.image.y - areas.mosaic.y, width: view.plan.image.w, height: view.plan.image.h, bottom: "auto", right: "auto" }
              : undefined
          }
        />
      ) : null}
      {view.reel?.show && view.plan.image && areas.mosaic ? (
        <Reel
          reel={view.reel}
          onDone={reelDone}
          box={{ left: view.plan.image.x - areas.mosaic.x, top: view.plan.image.y - areas.mosaic.y, width: view.plan.image.w, height: view.plan.image.h }}
          badgeTop={phone ? 10 : Math.max(10, HUD_BAND - (view.plan.image.y - areas.mosaic.y))}
        />
      ) : null}
    </div>
  );

  const keyGrid = view.layout ? (
    <div className="key-grid" role="group" aria-label={view.gridLabel} style={covered ? { visibility: "hidden" } : undefined}>
      {order.map((t) => {
        const tv = view.tiles.find((x) => x.id === t.id);
        if (!tv?.box) return null;
        return (
          <button
            key={t.id}
            ref={(el) => {
              if (el) gridRefs.current.set(t.id, el);
              else gridRefs.current.delete(t.id);
            }}
            type="button"
            tabIndex={t.id === keyTarget ? 0 : -1}
            aria-label={tv.ariaLabel}
            style={{ left: tv.box.x, top: tv.box.y, width: tv.box.w, height: tv.box.h }}
            onFocus={() => client.ui({ keyId: t.id })}
            onBlur={() => client.ui({ keyId: null })}
            onKeyDown={(e) => onGridKey(e, t.id)}
            onClick={(e) => openTile(t.id, e.currentTarget)}
          />
        );
      })}
    </div>
  ) : null;

  const restart = score && !score.button.hidden ? (
    <div className="restart">
      <button
        type="button"
        className="btn primary big"
        disabled={score.button.disabled}
        onClick={() => client.act("restart_now")}
        data-testid="restart-all"
        aria-describedby="restart-note"
      >
        <RestartIcon />
        {score.button.label}
      </button>
      {score.button.note ? (
        <span className="note" id="restart-note" data-testid="restart-note">
          {score.button.note}
        </span>
      ) : null}
    </div>
  ) : null;

  // A header button opens its own panel; pressing the one whose panel is open closes it, and
  // pressing another swaps the panel in place.
  const drawerOn = state.ui.drawer;
  const openDrawer = (panel: DrawerPanel, from: HTMLElement) => {
    if (drawerOn && drawerPanel === panel) {
      closeDrawer();
      return;
    }
    drawerOpener.current = from;
    setDrawerPanel(panel);
    client.ui({ drawer: true, lightMode: false });
  };
  const playOpen = drawerOn && drawerPanel === "play";
  // However the drawer closed, the next plain open is the controls.
  useEffect(() => {
    if (!drawerOn) setDrawerPanel("play");
  }, [drawerOn]);

  const needsVerifyPrompt = view.play?.verify.needed && state.turnstile.queued.length > 0 && !state.ui.drawer;

  const brand = (
    <div className="brand">
      <span className="site">{view.site}</span>
      <h1>{view.title}</h1>
      {view.live ? (
        <span className="live-badge" data-testid="live">
          <span className="dot" aria-hidden="true" />
          LIVE
        </span>
      ) : null}
    </div>
  );

  const next = score?.countdown.show ? (
    <span className={`next${score.countdown.racing ? " racing" : ""}`} data-testid="countdown">
      <span className="label">{score.countdown.label}</span> <span className="value" data-testid="countdown-value">{score.countdown.value}</span>
    </span>
  ) : null;

  // While a race runs the strip says who started it (the clocks are on the image); after
  // its result card, one line says how the last race went.
  const racing = view.race !== null;
  const info = (
    <div className="info">
      {view.race?.label ? (
        <span className="next racing" data-testid="countdown">
          {view.race.label}
        </span>
      ) : null}
      {!racing && view.lastRace ? (
        <span className="last-race" data-testid="last-race">
          {view.lastRace}
        </span>
      ) : null}
      {!racing ? next : null}
      {view.spec ? (
        <span className="spec" data-testid="spec">
          {view.spec}
        </span>
      ) : null}
    </div>
  );

  return (
    <>
      <canvas ref={canvasRef} className={`frames${painted ? " painted" : ""}`} aria-hidden="true" />
      {phone ? (
        <div className="phone" data-version={view.version} style={covered ? { visibility: "hidden" } : undefined}>
          <header>
            {brand}
            <div className="header-icons">
              <GithubLink href={view.isolation.link.href} label={GITHUB_LABEL} />
              <button type="button" className="icon-btn" aria-label="About this page" title="About this page" aria-expanded={drawerOn && drawerPanel === "about"} data-testid="menu" onClick={(e) => openDrawer("about", e.currentTarget)}>
                <MenuIcon />
              </button>
            </div>
          </header>
          {stage}
          {view.race && view.race.clocks.length > 0 ? (
            <div className="race-row">
              {view.race.clocks.map((c) => (
                <HalfClockView key={c.half} c={c} />
              ))}
            </div>
          ) : null}
          {view.sleep ? <WakeCard sleep={view.sleep} /> : null}
          {view.race?.end ? (
            <p className="end-line" data-testid="end-card">
              {view.race.end}
            </p>
          ) : null}
          {info}
          <p className="rotate-hint" data-testid="rotate-hint">
            {view.rotateHint}
          </p>
          {view.play ? (
            <button type="button" className="link play-link" data-testid="phone-play" aria-expanded={playOpen} onClick={(e) => openDrawer("play", e.currentTarget)}>
              {view.playButton.phoneLabel}
            </button>
          ) : null}
          <div className="dock">{restart}</div>
        </div>
      ) : (
        <main className={`wall${state.ui.returning ? " arrive" : ""}${idle ? " idle" : ""}`} data-version={view.version} style={covered ? { visibility: "hidden" } : undefined}>
          {stage}
          <header className="hud-top" data-hud="">
            {brand}
            <div className="actions fades">
              {view.viewers ? <span className="viewers" data-testid="viewers">{view.viewers}</span> : null}
              <button
                type="button"
                className="icon-btn"
                aria-pressed={view.pause.on}
                aria-label={view.pause.label}
                title={view.pause.label}
                onClick={() => client.ui({ paused: !view.pause.on })}
                data-testid="pause"
              >
                <PauseIcon paused={view.pause.on} />
              </button>
              {view.play ? (
                <button
                  ref={playButtonRef}
                  type="button"
                  className="btn"
                  aria-expanded={playOpen}
                  onClick={(e) => openDrawer("play", e.currentTarget)}
                  data-testid="play"
                >
                  {view.playButton.label}
                </button>
              ) : null}
              {view.ownButton ? (
                <span className="own-btn">
                  <button type="button" className="btn accent" disabled={view.ownButton.disabled} onClick={(e) => getOwnGpu(e.currentTarget)} data-testid="own-gpu" title={view.ownButton.note}>
                    {view.ownButton.label}
                  </button>
                  {view.ownButton.disabled ? <span className="note" data-testid="own-note">{view.ownButton.note}</span> : null}
                </span>
              ) : null}
              <button type="button" className="icon-btn" aria-label={view.isolation.open} title={view.isolation.open} aria-expanded={drawerOn && drawerPanel === "isolation"} onClick={(e) => openDrawer("isolation", e.currentTarget)} data-testid="isolation-link">
                <InfoIcon />
              </button>
              <GithubLink href={view.isolation.link.href} label={GITHUB_LABEL} />
              <button type="button" className="icon-btn" aria-label="About this page" title="About this page" aria-expanded={drawerOn && drawerPanel === "about"} onClick={(e) => openDrawer("about", e.currentTarget)} data-testid="menu">
                <MenuIcon />
              </button>
            </div>
          </header>
          <footer className="hud-bottom" data-hud="">
            <div className="strip">
              <div className="fades">{restart}</div>
              {info}
            </div>
          </footer>
        </main>
      )}
      {view.toast && !covered ? (
        <div className="toast" key={view.toast.key} data-testid="toast" role="status">
          {view.toast.text}
        </div>
      ) : null}
      {hint === "showing" && !covered && !phone ? (
        <div className="hint" data-testid="hint">
          {view.hint}
        </div>
      ) : null}
      <Overlays view={view} phone={phone} />
      {keyGrid}
      {state.ui.drawer && view.play && !covered ? <PlayDrawer client={client} view={view} onClose={closeDrawer} panel={drawerPanel} isolationInAbout={phone} word={wordDraft} setWord={setWordDraft} /> : null}
      {needsVerifyPrompt ? (
        <div className="verify-pop" role="dialog" aria-label={view.play!.verify.title}>
          <VerifyBox client={client} view={view} />
        </div>
      ) : null}
      {view.focus ? (
        <FocusPanel
          focus={view.focus}
          phone={phone}
          sheetOpen={sheetOpen}
          onSheet={setSheetChoice}
          onClose={closeTile}
          onRestart={() => client.act("relaunch", { tile: view.focus!.id })}
          onPick={pickZoomed}
          onMove={moveZoomed}
        />
      ) : null}
      {view.own ? <OwnGpu own={view.own} client={client} shotRef={ownShotRef} onClose={closeOwn} /> : null}
      {view.ownRefusal ? (
        <div className="refusal" role="alertdialog" aria-labelledby="refusal-text" data-testid="own-refused">
          <p id="refusal-text">{view.ownRefusal.text}</p>
          <button type="button" className="btn" autoFocus onClick={() => client.dispatch({ type: "own-closed" })}>
            {view.ownRefusal.dismiss}
          </button>
        </div>
      ) : null}
      <div className="sr-only" aria-live="polite" data-testid="announce">
        {score?.announce ?? ""}
      </div>
    </>
  );
}
