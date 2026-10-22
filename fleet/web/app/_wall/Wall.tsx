"use client";

// The wall page. It measures the screen, forwards input as actions, and draws
// the view model core/ computes. It computes nothing itself.

import { useCallback, useEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";
import {
  addDrag,
  CLICK_SLOP_PX,
  emptyDrag,
  LONG_PRESS_MS,
  neighbour,
  readingOrder,
  screenToImage,
  takeDrag,
  tileAt,
  wheelDive,
  lightPosition,
  sceneT,
  serverNow,
  LIGHT_COLORS,
  wallView,
  type Box,
  type Direction,
  type DragAccumulator,
  type WallView,
} from "../../core/index.ts";
import { canDecodeH264, WallClient } from "./wall-client.ts";
import { Controls, PhoneDock } from "./Controls.tsx";
import { FocusPanel } from "./FocusPanel.tsx";
import { Overlays } from "./Overlays.tsx";
import type { DrawSettings } from "./worker-msgs.ts";

const localNow = () => performance.timeOrigin + performance.now();

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


export function Wall({ coordUrl }: { coordUrl: string }) {
  const client = useMemo(() => new WallClient(coordUrl), [coordUrl]);
  const state = useSyncExternalStore(client.subscribe, client.getState, client.getState);
  const [now, setNow] = useState(0);
  const [canDecode, setCanDecode] = useState(true);
  const [dpr, setDpr] = useState(1);
  const [viewport, setViewport] = useState({ w: 0, h: 0 });
  const [areas, setAreas] = useState<{ mosaic: Box | null; focus: Box | null }>({ mosaic: null, focus: null });
  const phone = useMedia("(max-width: 700px)");
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const stageRef = useRef<HTMLDivElement | null>(null);
  const focusShotRef = useRef<HTMLDivElement | null>(null);
  const viewRef = useRef<WallView | null>(null);
  const gridRefs = useRef(new Map<string, HTMLButtonElement>());
  const returnFocus = useRef<HTMLElement | null>(null);

  // Clock, capability, connection.
  useEffect(() => {
    setNow(localNow());
    const id = setInterval(() => setNow(localNow()), 100);
    client.start();
    void canDecodeH264().then(setCanDecode);
    return () => {
      clearInterval(id);
      client.stop();
    };
  }, [client]);

  // Screen size and the measured areas.
  useEffect(() => {
    const measure = () => {
      setDpr(window.devicePixelRatio || 1);
      setViewport((v) => (v.w === window.innerWidth && v.h === window.innerHeight ? v : { w: window.innerWidth, h: window.innerHeight }));
      setAreas((a) => {
        const mosaic = boxOf(stageRef.current);
        const focus = boxOf(focusShotRef.current);
        return sameBox(a.mosaic, mosaic) && sameBox(a.focus, focus) ? a : { mosaic, focus };
      });
    };
    measure();
    const ro = new ResizeObserver(measure);
    if (stageRef.current) ro.observe(stageRef.current);
    if (focusShotRef.current) ro.observe(focusShotRef.current);
    window.addEventListener("resize", measure);
    const mq = window.matchMedia(`(resolution: ${window.devicePixelRatio}dppx)`);
    mq.addEventListener("change", measure);
    return () => {
      ro.disconnect();
      window.removeEventListener("resize", measure);
      mq.removeEventListener("change", measure);
    };
  }, [phone, state.ui.focusId, state.accepted === null]);

  const view = useMemo(
    () =>
      wallView({
        state,
        now: now || 0,
        dpr,
        mosaicArea: areas.mosaic,
        focusArea: state.ui.focusId ? areas.focus : null,
        canDecode,
        phone,
      }),
    [state, now, dpr, areas, canDecode, phone],
  );
  viewRef.current = view;

  // The frame worker gets the canvas once, then plans.
  useEffect(() => {
    const c = canvasRef.current;
    if (!c || !canDecode || viewport.w === 0) return;
    client.attachCanvas(c, viewport.w, viewport.h, dpr);
  }, [client, canDecode, viewport.w, viewport.h, dpr]);
  useEffect(() => {
    client.resize(viewport.w, viewport.h, dpr);
  }, [client, viewport.w, viewport.h, dpr]);
  useEffect(() => {
    const settings: DrawSettings = {
      background: view.focus ? "#0B0D10" : phone ? "#0B0D10" : "#000000",
      dimAlpha: 0.5,
      fadeInMs: 280,
    };
    client.plan(view.plan, settings);
  }, [client, view.plan, view.focus, phone]);

  // Test hooks, only with ?debug=1.
  useEffect(() => {
    if (new URLSearchParams(window.location.search).get("debug") === "1") {
      window.__fleet = { client, view: () => viewRef.current! };
    }
  }, [client]);

  // ---- pointer input on the image ----------------------------------------------

  const drag = useRef<{ id: number; x: number; y: number; moved: boolean; acc: DragAccumulator; longPress: ReturnType<typeof setTimeout> | null; pressedTile: string | null } | null>(null);
  const [dragging, setDragging] = useState(false);

  const imagePoint = (x: number, y: number) => {
    const L = viewRef.current?.layout;
    return L ? screenToImage(L.image, x, y) : null;
  };
  const tileUnder = (x: number, y: number) => {
    const p = imagePoint(x, y);
    const tiles = client.getState().accepted?.tiles ?? [];
    return p ? tileAt(tiles, p.u, p.v) : null;
  };
  const flushDrag = (force: boolean) => {
    const d = drag.current;
    if (!d) return;
    const r = takeDrag(d.acc, localNow(), force);
    d.acc = r.acc;
    if (r.args) client.send("camera", r.args);
  };

  const onPointerDown = (e: React.PointerEvent) => {
    if (view.focus || e.button !== 0) return;
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    const tile = tileUnder(e.clientX, e.clientY);
    drag.current = {
      id: e.pointerId,
      x: e.clientX,
      y: e.clientY,
      moved: false,
      acc: emptyDrag(),
      pressedTile: tile?.id ?? null,
      longPress:
        e.pointerType === "touch" && tile
          ? setTimeout(() => {
              client.ui({ hoverId: tile.id, pointerOverImage: true });
              if (drag.current) drag.current.longPress = null;
              if (drag.current) drag.current.pressedTile = "__long__";
            }, LONG_PRESS_MS)
          : null,
    };
  };

  const onPointerMove = (e: React.PointerEvent) => {
    const d = drag.current;
    if (d && d.id === e.pointerId) {
      const dx = e.clientX - d.x;
      const dy = e.clientY - d.y;
      if (!d.moved && Math.hypot(dx, dy) > CLICK_SLOP_PX && d.pressedTile !== "__long__") {
        d.moved = true;
        if (d.longPress) clearTimeout(d.longPress);
        d.longPress = null;
        setDragging(true);
      }
      if (d.moved) {
        const img = viewRef.current?.layout?.image;
        if (img) d.acc = addDrag(d.acc, dx / img.w, dy / img.h);
        d.x = e.clientX;
        d.y = e.clientY;
        flushDrag(false);
      }
      return;
    }
    if (e.pointerType === "touch") return;
    const tile = tileUnder(e.clientX, e.clientY);
    const over = imagePoint(e.clientX, e.clientY) !== null;
    const s = client.getState().ui;
    if (s.hoverId !== (tile?.id ?? null) || s.pointerOverImage !== over) client.ui({ hoverId: tile?.id ?? null, pointerOverImage: over });
  };

  const endPointer = (e: React.PointerEvent, cancelled: boolean) => {
    const d = drag.current;
    if (!d || d.id !== e.pointerId) return;
    if (d.longPress) clearTimeout(d.longPress);
    // Send what the drag moved since its last send before letting it go.
    if (d.moved) flushDrag(true);
    drag.current = null;
    setDragging(false);
    if (d.moved) return;
    if (d.pressedTile === "__long__") {
      client.ui({ hoverId: null, pointerOverImage: false });
      return;
    }
    if (cancelled) return;
    const s = client.getState();
    if (s.ui.lightMode) {
      const p = imagePoint(e.clientX, e.clientY);
      const st = s.accepted;
      if (p && st) {
        // The world point under the click, on the camera as it was drawn.
        const t = s.frames?.T ?? sceneT(st.scene.epoch_unix_ms, serverNow(s.clock, localNow()));
        const pos = lightPosition(st.scene.camera, t, p.u, p.v, st.image_aspect);
        const color = LIGHT_COLORS[st.scene.lights.length % LIGHT_COLORS.length]!;
        if (pos) client.send("light", { pos, color });
      }
      client.ui({ lightMode: false });
      return;
    }
    const tile = tileUnder(e.clientX, e.clientY);
    if (tile) openTile(tile.id, null);
  };

  const onPointerLeave = (e: React.PointerEvent) => {
    if (e.pointerType === "touch" || drag.current) return;
    client.ui({ hoverId: null, pointerOverImage: false });
  };

  const wheel = useRef<DragAccumulator>(emptyDrag());
  const onWheel = (e: React.WheelEvent) => {
    if (view.focus) return;
    wheel.current = addDrag(wheel.current, 0, 0, wheelDive(e.deltaY));
    const r = takeDrag(wheel.current, localNow());
    wheel.current = r.acc;
    if (r.args) client.send("camera", r.args);
  };

  // ---- open and close a tile -------------------------------------------------------

  const openTile = (id: string, from: HTMLElement | null) => {
    returnFocus.current = from ?? (document.activeElement as HTMLElement | null);
    client.ui({ focusId: id, hoverId: null, pointerOverImage: false });
  };
  const closeTile = () => {
    client.ui({ focusId: null });
    const el = returnFocus.current;
    returnFocus.current = null;
    requestAnimationFrame(() => el?.focus());
  };

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

  const c = view.controls;
  const stageClass = `stage${dragging ? " dragging" : ""}${state.ui.lightMode ? " light-mode" : ""}`;
  const stage = (
    <div
      ref={stageRef}
      className={stageClass}
      data-testid="stage"
      data-version={view.version}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={(e) => endPointer(e, false)}
      onPointerCancel={(e) => endPointer(e, true)}
      onPointerLeave={onPointerLeave}
      onWheel={onWheel}
      onContextMenu={(e) => e.preventDefault()}
    />
  );

  const keyGrid = view.layout ? (
    <div className="key-grid" role="group" aria-label={view.gridLabel} style={view.focus ? { visibility: "hidden" } : undefined}>
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

  return (
    <>
      <canvas ref={canvasRef} className="frames" aria-hidden="true" />
      {phone ? (
        <div className="phone" data-version={view.version} style={view.focus ? { visibility: "hidden" } : undefined}>
          <header>
            <div className="site">{view.site}</div>
            <h1>{view.title}</h1>
            {view.phone ? (
              <div className="kpis">
                <div>
                  <span>{view.phone.up.label}</span>
                  <span>{view.phone.up.value}</span>
                </div>
                <div>
                  <span style={{ color: "var(--fcvm)" }}>{view.phone.fcvm.label}</span>
                  <span style={{ color: "var(--fcvm)" }}>{view.phone.fcvm.value}</span>
                </div>
                <div>
                  <span style={{ color: "var(--ec2)" }}>{view.phone.ec2.label}</span>
                  <span style={{ color: "var(--ec2)" }}>{view.phone.ec2.value}</span>
                </div>
              </div>
            ) : null}
          </header>
          {stage}
          {view.race ? (
            <div className="strip" aria-label="Relaunch all race">
              <span style={{ color: "var(--fcvm)" }}>{view.race.fcvm.back}</span>
              <span className="clock">{view.race.clock}</span>
              <span style={{ color: "var(--ec2)" }}>{view.race.ec2.back}</span>
            </div>
          ) : null}
          <div className="list">
            {view.tiles.map((t) => (
              <button
                key={t.id}
                type="button"
                className="tile-card"
                style={{ ["--accent" as string]: t.accent }}
                aria-label={t.ariaLabel}
                onClick={(e) => openTile(t.id, e.currentTarget)}
              >
                <span className="row">
                  <span className="tag">{t.hover.kind}</span>
                  <span className="time">{t.dark ? t.darkClock : t.lastShort}</span>
                </span>
                <span className="name">{t.name}</span>
                <span className="status">{t.status}</span>
              </button>
            ))}
            {c ? (
              <details className="more">
                <summary>Scene and stats</summary>
                <Controls client={client} view={view} compact />
              </details>
            ) : null}
          </div>
          {c ? <PhoneDock client={client} view={view} /> : null}
        </div>
      ) : (
        <main className="wall" data-version={view.version} style={view.focus ? { visibility: "hidden" } : undefined}>
          {stage}
          {c ? <Controls client={client} view={view} compact={false} /> : null}
        </main>
      )}
      <Overlays view={view} phone={phone} />
      {keyGrid}
      {view.focus ? (
        <FocusPanel
          view={view}
          focus={view.focus}
          shotRef={focusShotRef}
          onClose={closeTile}
          onRelaunch={() => client.send("relaunch", { tile: view.focus!.id })}
        />
      ) : null}
      <span className="sr-only" aria-live="polite">
        {view.race?.active ? `${view.race.fcvm.back} fcvm, ${view.race.ec2.back} EC2` : ""}
      </span>
    </>
  );
}

