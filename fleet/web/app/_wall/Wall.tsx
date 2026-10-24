"use client";

// The wall page: a stage, not a dashboard. The image sits letterboxed between
// the race (countdown and two bars) above and the one button below; everything
// else is in the Play drawer. The page measures the screen, forwards input as
// actions, and draws the view model core/ computes. It computes nothing itself.

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
  readingOrder,
  screenToImage,
  pointerTile,
  lightPosition,
  sceneT,
  serverNow,
  LIGHT_COLORS,
  wallView,
  type Box,
  type Direction,
  type WallView,
} from "../../core/index.ts";
import { canDecodeH264, WallClient } from "./wall-client.ts";
import { FocusPanel } from "./FocusPanel.tsx";
import { OwnGpu } from "./OwnGpu.tsx";
import { Overlays } from "./Overlays.tsx";
import { PlayDrawer, VerifyBox } from "./PlayDrawer.tsx";
import { Reel } from "./Reel.tsx";
import { Countdown, Scoreboard } from "./Score.tsx";
import type { DrawSettings } from "./worker-msgs.ts";

const localNow = () => performance.timeOrigin + performance.now();
const MODE_KEY = "fleet-wall.mode";

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

type Areas = { mosaic: Box | null; focus: Box | null; own: Box | null };

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
  const state = useSyncExternalStore(client.subscribe, client.getState, client.getState);
  const [now, setNow] = useState(0);
  const [canDecode, setCanDecode] = useState(true);
  const [painted, setPainted] = useState(false);
  // The recording's metadata: null until read, or when it is missing.
  const [reelMeta, setReelMeta] = useState<ReelMeta | null>(null);
  const [dpr, setDpr] = useState(1);
  const [viewport, setViewport] = useState({ w: 0, h: 0 });
  const [areas, setAreas] = useState<Areas>({ mosaic: null, focus: null, own: null });
  const [avoid, setAvoid] = useState<Box[]>([]);
  const [labelSize, setLabelSize] = useState<{ w: number; h: number } | undefined>(undefined);
  /** The drawer section to show when it opens (the isolation link), or null for its top. */
  const [drawerSection, setDrawerSection] = useState<string | null>(null);
  const phone = useMedia("(max-width: 700px)");
  const reducedMotion = useMedia("(prefers-reduced-motion: reduce)");
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const stageRef = useRef<HTMLDivElement | null>(null);
  const focusShotRef = useRef<HTMLDivElement | null>(null);
  const ownShotRef = useRef<HTMLDivElement | null>(null);
  const viewRef = useRef<WallView | null>(null);
  const gridRefs = useRef(new Map<string, HTMLButtonElement>());
  const returnFocus = useRef<HTMLElement | null>(null);
  const playButtonRef = useRef<HTMLButtonElement | null>(null);

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
        const next = { mosaic: boxOf(stageRef.current), focus: boxOf(focusShotRef.current), own: boxOf(ownShotRef.current) };
        return sameBox(a.mosaic, next.mosaic) && sameBox(a.focus, next.focus) && sameBox(a.own, next.own) ? a : next;
      });
    };
    measure();
    const ro = new ResizeObserver(measure);
    for (const el of [stageRef.current, focusShotRef.current, ownShotRef.current]) if (el) ro.observe(el);
    window.addEventListener("resize", measure);
    const mq = window.matchMedia(`(resolution: ${window.devicePixelRatio}dppx)`);
    mq.addEventListener("change", measure);
    return () => {
      ro.disconnect();
      window.removeEventListener("resize", measure);
      mq.removeEventListener("change", measure);
    };
  }, [phone, state.ui.focusId, ownOpen, state.ui.drawer, state.accepted === null]);

  const view = useMemo(
    () =>
      wallView({
        state,
        now: now || 0,
        dpr,
        mosaicArea: areas.mosaic,
        focusArea: state.ui.focusId ? areas.focus : null,
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
    [state, now, dpr, areas, canDecode, phone, reducedMotion, client, avoid, labelSize, poster, ownOpen, mediaUrl, reelMeta],
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
  const covered = view.focus !== null || view.own !== null;
  useEffect(() => {
    const settings: DrawSettings = { background: covered ? "#0B0D10" : "#000000", dimAlpha: 0.5, fadeInMs: 280 };
    client.plan(view.plan, settings);
  }, [client, view.plan, covered]);

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
              client.ui({ hoverId: tile.id, pointerOverImage: true });
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
    if (s.hoverId !== (tile?.id ?? null) || s.pointerOverImage !== over) client.ui({ hoverId: tile?.id ?? null, pointerOverImage: over });
  };

  const endPointer = (e: React.PointerEvent, cancelled: boolean) => {
    const d = press.current;
    if (!d || d.id !== e.pointerId) return;
    if (d.longPress) clearTimeout(d.longPress);
    press.current = null;
    if (d.long) {
      client.ui({ hoverId: null, pointerOverImage: false });
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
  const closeTile = useCallback(() => {
    client.ui({ focusId: null });
    restoreFocus();
  }, [client]);
  const closeDrawer = useCallback(() => {
    client.ui({ drawer: false });
    requestAnimationFrame(() => playButtonRef.current?.focus());
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
      {poster ? (
        <img
          className={`poster${painted ? " gone" : ""}`}
          src={poster}
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

  const restart = score ? (
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
      <span className="note" id="restart-note" data-testid="restart-note">
        {score.button.note}
      </span>
    </div>
  ) : null;

  const needsVerifyPrompt = view.play?.verify.needed && state.turnstile.queued.length > 0 && !state.ui.drawer;

  return (
    <>
      <canvas ref={canvasRef} className={`frames${painted ? " painted" : ""}`} aria-hidden="true" />
      {phone ? (
        <div className="phone" data-version={view.version} style={covered ? { visibility: "hidden" } : undefined}>
          <header>
            <span className="site">{view.site}</span>
            <h1>{view.title}</h1>
            <p className="sentence">{view.sentence}</p>
          </header>
          {score ? <Countdown score={score} counter={view.counter} /> : null}
          {score ? <Scoreboard score={score} compact /> : null}
          {stage}
          <p className="rotate-hint" data-testid="rotate-hint">
            {view.rotateHint}
          </p>
          {view.play ? (
            <button type="button" className="link play-link" data-testid="phone-play" aria-expanded={view.playButton.open} onClick={() => client.ui({ drawer: true, lightMode: false })}>
              {view.playButton.phoneLabel}
            </button>
          ) : null}
          <div className="dock">{restart}</div>
        </div>
      ) : (
        <main className={`wall${state.ui.returning ? " arrive" : ""}`} data-version={view.version} style={covered ? { visibility: "hidden" } : undefined}>
          <header className="top">
            <div className="brand">
              <span className="site">{view.site}</span>
              <h1>{view.title}</h1>
              <p className="sentence" data-testid="sentence">{view.sentence}</p>
            </div>
            <div className="actions">
              {view.viewers ? <span className="viewers" data-testid="viewers">{view.viewers}</span> : null}
              <button type="button" className="btn" aria-pressed={view.pause.on} onClick={() => client.ui({ paused: !view.pause.on })} data-testid="pause">
                {view.pause.label}
              </button>
              {view.play ? (
                <button
                  ref={playButtonRef}
                  type="button"
                  className="btn"
                  aria-expanded={view.playButton.open}
                  onClick={() => {
                    setDrawerSection(null);
                    client.ui({ drawer: !view.playButton.open, lightMode: false });
                  }}
                  data-testid="play"
                >
                  {view.playButton.label}
                </button>
              ) : null}
              {view.ownButton ? (
                <span className="own-btn">
                  <button type="button" className="btn accent" disabled={view.ownButton.disabled} onClick={(e) => getOwnGpu(e.currentTarget)} data-testid="own-gpu">
                    {view.ownButton.label}
                  </button>
                  <span className="note" data-testid="own-note">{view.ownButton.note}</span>
                </span>
              ) : null}
            </div>
          </header>
          {score ? (
            <section className="score" aria-label="The race">
              <Countdown
                score={score}
                counter={view.counter}
                isolation={{
                  label: view.isolation.open,
                  onOpen: () => {
                    setDrawerSection("isolation");
                    client.ui({ drawer: true, lightMode: false });
                  },
                }}
              />
              <Scoreboard score={score} compact={false} />
            </section>
          ) : (
            <section className="score" aria-hidden="true" />
          )}
          {stage}
          <footer className="bottom">
            {restart}
            <ol className="feed" aria-label="Recent actions" data-testid="feed">
              {view.feed.map((a) => (
                <li key={a.key}>
                  {a.text} <span className="when">{a.when}</span>
                </li>
              ))}
            </ol>
            <span className="help">{view.help}</span>
          </footer>
        </main>
      )}
      <Overlays view={view} phone={phone} />
      {keyGrid}
      {state.ui.drawer && view.play && !covered ? <PlayDrawer client={client} view={view} onClose={closeDrawer} section={drawerSection} /> : null}
      {needsVerifyPrompt ? (
        <div className="verify-pop" role="dialog" aria-label={view.play!.verify.title}>
          <VerifyBox client={client} view={view} />
        </div>
      ) : null}
      {view.focus ? (
        <FocusPanel focus={view.focus} shotRef={focusShotRef} onClose={closeTile} onRestart={() => client.act("relaunch", { tile: view.focus!.id })} />
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
