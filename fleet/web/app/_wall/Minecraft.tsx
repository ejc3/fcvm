"use client";

// Minecraft mode (`?mode=minecraft`): every server's world drawn in 3D from the tile's view socket
// (minecraft-stage.ts; a flat map on the tile's own canvas where the browser has no WebGL2), with
// the header's figures, Reset all, and per tile Reset, Fork and Take over. The server this viewer holds is driven from the keyboard (or the on-screen
// buttons of a touch layout) over that tile's socket. The page measures the stage, ticks a clock
// and draws the view model core/minecraft.ts computes; every word on it comes from there.

import { useCallback, useEffect, useMemo, useRef, useState, useSyncExternalStore, type CSSProperties } from "react";
import {
  cellAt,
  cellText,
  driveInputOf,
  FIELD_LEGEND,
  FIELD_LEGEND_H,
  FIELD_LEGEND_RING,
  FIELD_LEGEND_YOUNG,
  FIELD_READOUT_H,
  FIELD_READOUT_H_NARROW,
  FIELD_TEXT_H,
  FIELD_TEXT_H_NARROW,
  moveCursor,
  scrollTo,
  type SampleSlot,
  type WaveFrozen,
  Driver,
  minecraftView,
  SITE,
  type Box,
  type DriveInput,
  type McRenderer,
  type MinecraftCard,
  type MinecraftPanel,
  type MinecraftView,
} from "../../core/index.ts";
import { WallClient } from "./wall-client.ts";
import { McTiles } from "./minecraft-tiles.ts";
import { McStage, type StageDebug } from "./minecraft-stage.ts";
import { McField } from "./minecraft-field.ts";
import { VerifyCard } from "./Turnstile.tsx";
import { ModeSwitch } from "./ModeSwitch.tsx";
import { WakeCard } from "./Overlays.tsx";
import { boxOf, sameBox, useMedia } from "./hooks.ts";
import "./windows.css";
import "./minecraft.css";

const localNow = () => performance.timeOrigin + performance.now();

declare global {
  interface Window {
    /** With ?debug=1 the Minecraft page exposes its client, its view, the tiles' sockets and the take-over's driver. */
    __minecraft?: {
      client: WallClient;
      view: () => MinecraftView;
      tiles: McTiles;
      driver: () => { samples: number[]; yaw: number; keys: string[] } | null;
      /** The 3D view's counters; null with the flat map. */
      stage: () => StageDebug | null;
      /** Turn the frame-time guard off or on (a test that looks at motion on a software renderer). */
      setBudget: (on: boolean) => void;
      /** The field's painter: how often it painted, and how many cells its last paint drew. */
      field: () => { paints: number; cells: number };
    };
  }
}


const at = (b: Box): CSSProperties => ({ left: b.x, top: b.y, width: b.w, height: b.h });

/** Keep a held button's or a drag's pointer: its release then reaches the element that took the press. */
function capture(el: Element, pointerId: number): void {
  try {
    el.setPointerCapture(pointerId);
  } catch {
    // A pointer that is no longer active cannot be captured; the press still counts.
  }
}

function Caption({ c, onReset, onFork, onTake, onClose, onUnpin }: { c: MinecraftCard; onReset: () => void; onFork: () => void; onTake: () => void; onClose: () => void; onUnpin: () => void }) {
  if (!c.caption) return null;
  return (
    <div className={`mc-caption${c.compact ? " compact" : ""}`} style={at(c.caption)} data-testid={`mc-caption-${c.slot}`} data-mine={c.mine ? "true" : undefined} data-lent={c.lent ? "true" : undefined}>
      <div className="row">
        <strong className="slot" data-testid={`mc-slot-${c.slot}`}>
          {c.slot}
        </strong>
        <span className="tps" data-testid={`mc-tps-${c.slot}`}>
          {c.tps}
        </span>
        {c.upIn ? (
          <span className="in" data-testid={`mc-up-in-${c.slot}`}>
            {c.upIn}
          </span>
        ) : null}
        {/* a server a viewer picked from the field: marked, and given back to the rotation here */}
        {c.pinned && !c.opened ? (
          <button type="button" className="btn mini pin" title="Picked from the field: it keeps this tile for a minute." onClick={onUnpin} data-testid={`mc-unpin-${c.slot}`}>
            Unpin
          </button>
        ) : null}
      </div>
      {c.forkedFrom ? (
        <p className="forked" title={c.forkSplit ?? undefined} data-testid={`mc-forked-${c.slot}`}>
          {c.forkedFrom}
          {c.forkSplit && !c.compact ? <span className="split"> ({c.forkSplit})</span> : null}
        </p>
      ) : null}
      {c.showActions ? (
        <div className="buttons" role="group" aria-label={`${c.slot} actions`}>
          <button type="button" className="btn" disabled={c.reset.disabled} title={c.reset.title ?? undefined} onClick={onReset} data-testid={`mc-reset-${c.slot}`}>
            {c.reset.label}
          </button>
          <button type="button" className="btn" disabled={c.fork.disabled} title={c.fork.title ?? undefined} onClick={onFork} data-testid={`mc-fork-${c.slot}`}>
            {c.fork.label}
          </button>
          <button type="button" className="btn" disabled={c.take.disabled} title={c.take.title ?? undefined} onClick={onTake} data-testid={`mc-take-${c.slot}`}>
            {c.take.label}
          </button>
          {c.opened ? (
            <button type="button" className="btn" onClick={onClose} data-testid="mc-close">
              All servers
            </button>
          ) : null}
        </div>
      ) : null}
      {c.note ? (
        <p className="note" data-testid={`mc-note-${c.slot}`}>
          {c.note}
        </p>
      ) : null}
      {c.about ? (
        <p className="about" data-testid={`mc-about-${c.slot}`}>
          {c.about}
        </p>
      ) : null}
      {!c.compact && c.lentText ? (
        <p className="lent" data-testid={`mc-lent-${c.slot}`}>
          {c.lentText}
        </p>
      ) : null}
      {!c.compact && !c.lentText && c.join ? (
        <p className="join" data-testid={`mc-join-${c.slot}`}>
          {c.join}
        </p>
      ) : null}
    </div>
  );
}

/** One line for Java Edition players: the client, one server's address to copy, and where the others' are. */
function JoinLine({ join }: { join: { label: string; address: string } }) {
  const [copied, setCopied] = useState(false);
  const copy = () => {
    void navigator.clipboard?.writeText(join.address).then(
      () => {
        setCopied(true);
        setTimeout(() => setCopied(false), 1500);
      },
      () => setCopied(false),
    );
  };
  return (
    <p className="mc-join" data-testid="mc-join">
      <span className="label">{join.label}</span>
      <code data-testid="mc-join-address">{join.address}</code>
      <button type="button" className="btn mini" onClick={copy} data-testid="mc-join-copy">
        {copied ? "Copied" : "Copy"}
      </button>
      <span className="mc-hint" data-testid="mc-join-hint">Each server's own address is under its tile.</span>
    </p>
  );
}

function Sheet({
  panel,
  narrow,
  top,
  onGiveBack,
  onPad,
  onRetry,
}: {
  panel: MinecraftPanel;
  narrow: boolean;
  top: number;
  onGiveBack: () => void;
  onPad: (input: DriveInput, down: boolean) => void;
  onRetry: () => void;
}) {
  return (
    <aside className={`win-sheet mc-sheet${narrow ? " narrow" : ""}`} role="dialog" aria-labelledby="mc-sheet-title" data-testid="mc-panel" style={top > 0 ? { top } : undefined}>
      <div className="win-sheet-body">
        <section className="card">
          <div className="head">
            <h2 id="mc-sheet-title" data-testid="mc-panel-title">
              {panel.slot}
            </h2>
            <span className="total" data-testid="mc-countdown">
              {panel.countdown}
            </span>
          </div>
          <p className="note" data-testid="mc-keys">
            {panel.keys}
          </p>
          <p className="first-change" data-testid="mc-first-change">
            {panel.firstChange}
          </p>
          {panel.waiting ? (
            <p className={panel.failed ? "refused" : "note"} role="status" data-testid="mc-waiting">
              {panel.waiting}
            </p>
          ) : null}
          {panel.failed ? (
            <button type="button" className="btn" style={{ justifyContent: "center" }} onClick={onRetry} data-testid="mc-retry">
              Retry
            </button>
          ) : null}
          {panel.refused ? (
            <p className="refused" role="status" data-testid="mc-drive-refused">
              {panel.refused}
            </p>
          ) : null}
          {panel.pad ? (
            <div className="mc-pad" role="group" aria-label="Drive" data-testid="mc-pad">
              {panel.pad.map((p) => (
                <button
                  key={p.input}
                  type="button"
                  className="btn"
                  disabled={!panel.driving}
                  data-testid={`mc-pad-${p.input}`}
                  onPointerDown={(e) => {
                    capture(e.currentTarget, e.pointerId);
                    onPad(p.input, true);
                  }}
                  onPointerUp={() => onPad(p.input, false)}
                  onPointerCancel={() => onPad(p.input, false)}
                  onContextMenu={(e) => e.preventDefault()}
                >
                  {p.label}
                </button>
              ))}
            </div>
          ) : null}
          <button type="button" className="btn" style={{ justifyContent: "center" }} onClick={onGiveBack} data-testid="mc-give-back">
            {panel.giveBack}
          </button>
        </section>
      </div>
    </aside>
  );
}

export function Minecraft({ coordUrl, directUrl }: { coordUrl: string; directUrl: string | null }) {
  // No frame streams: each tile is drawn from its own view socket.
  // This page shows Minecraft mode: it asks for the servers' status rows with its visibility.
  const client = useMemo(() => new WallClient(coordUrl, (st) => st.minecraft?.tiles ?? [], directUrl, true), [coordUrl, directUrl]);
  const fieldPainter = useMemo(() => new McField(), []);
  const fieldCanvas = useRef<HTMLCanvasElement | null>(null);
  // Servers picked from the field (each keeps a 3D tile for a while), the cell pointed at, and
  // which server each 3D tile showed last (the view steps from it, so a tile keeps its server).
  const [pins, setPins] = useState<{ n: number; at: number }[]>([]);
  const [cursor, setCursor] = useState<number | null>(null);
  const sample = useRef<SampleSlot[]>([]);
  // A reset of all once it is over, as it was shown then: kept until another reset, so later
  // launches (one server reset, a fork) never bring its line back or change its time.
  const waveFrozen = useRef<WaveFrozen | null>(null);
  const fieldScroll = useRef<HTMLDivElement | null>(null);
  // A mouse picks with its click; a finger's tap only points (the cells are smaller than a finger), and Show picks.
  const lastPointer = useRef("mouse");
  const tiles = useMemo(() => new McTiles(), []);
  const state = useSyncExternalStore(client.subscribe, client.getState, client.getState);
  const live = useSyncExternalStore(tiles.subscribe, tiles.getLive, tiles.getLive);
  const [now, setNow] = useState(0);
  const [dpr, setDpr] = useState(1);
  const [stage, setStage] = useState<Box | null>(null);
  const [samples, setSamples] = useState<number[]>([]);
  const [refused, setRefused] = useState<string | null>(null);
  const narrow = useMedia("(max-width: 700px)");
  const coarse = useMedia("(pointer: coarse)");
  const reducedMotion = useMedia("(prefers-reduced-motion: reduce)");
  // What draws the worlds: unknown until the page has asked for a WebGL2 context.
  const [renderer, setRenderer] = useState<McRenderer | null>(null);
  const [opened, setOpened] = useState<string | null>(null);
  // The flat map is shown because the 3D view was lost; the 3D view is drawn with less detail.
  const [lost, setLost] = useState(false);
  const [reduced, setReduced] = useState(false);
  const stageBox = useRef<Box | null>(null);
  const wasOpened = useRef<string | null>(null);
  const glRef = useRef<HTMLCanvasElement | null>(null);
  const flightsRef = useRef<HTMLDivElement | null>(null);
  const stage3d = useRef<McStage | null>(null);
  const touch = narrow || coarse;
  const stageRef = useRef<HTMLDivElement | null>(null);
  const viewRef = useRef<MinecraftView | null>(null);
  const canvases = useRef(new Map<string, HTMLCanvasElement>());
  const drawn = useRef(new Map<string, string>());
  const driverRef = useRef<{ session: string; id: string; driver: Driver } | null>(null);
  const pumpTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  // The session this page gave back: its panel closes at once, not when the coordinator says so.
  const [released, setReleased] = useState<string | null>(null);

  useEffect(() => {
    document.title = "Minecraft";
  }, []);

  // Clock and connection. The tiles' timers count tenths of a second.
  useEffect(() => {
    setNow(localNow());
    const id = setInterval(() => {
      setNow(localNow());
      setReduced(stage3d.current?.reduced ?? false);
    }, 100);
    client.start();
    return () => {
      clearInterval(id);
      client.stop();
      tiles.stop();
    };
  }, [client, tiles]);

  // The 3D view, where the browser has WebGL2; else, or once the context is lost, the flat map.
  useEffect(() => {
    const canvas = glRef.current;
    if (!canvas) return;
    const debug = new URLSearchParams(window.location.search).get("debug") === "1";
    let st: McStage | null = null;
    const start = () => {
      st = McStage.create(canvas, tiles, debug, () => {
        // The context is gone: the flat map takes over, and says why. The canvas stays, so the
        // browser can give the context back.
        st?.dispose();
        st = stage3d.current = null;
        setLost(true);
        setRenderer("flat");
      });
      stage3d.current = st;
      setRenderer(st === null ? "flat" : "3d");
      if (st !== null) setLost(false);
    };
    start();
    const restored = () => {
      if (st === null) start();
    };
    canvas.addEventListener("webglcontextrestored", restored);
    return () => {
      canvas.removeEventListener("webglcontextrestored", restored);
      st?.dispose();
      stage3d.current = null;
    };
  }, [tiles]);

  // Hidden tabs do not count as viewers, and hold nothing on the GPU.
  useEffect(() => {
    const send = () => {
      client.setVisible(document.visibilityState === "visible");
      if (document.visibilityState !== "visible") stage3d.current?.release();
    };
    send();
    document.addEventListener("visibilitychange", send);
    return () => document.removeEventListener("visibilitychange", send);
  }, [client]);

  // Screen size and the stage.
  useEffect(() => {
    const measure = () => {
      setDpr(window.devicePixelRatio || 1);
      setStage((a) => {
        const next = boxOf(stageRef.current);
        return sameBox(a, next) ? a : next;
      });
    };
    measure();
    const ro = new ResizeObserver(measure);
    if (stageRef.current) ro.observe(stageRef.current);
    window.addEventListener("resize", measure);
    return () => {
      ro.disconnect();
      window.removeEventListener("resize", measure);
    };
  }, [narrow]);

  const view = useMemo(
    () =>
      minecraftView({
        state,
        now: now || 0,
        dpr,
        stage,
        narrow,
        touch,
        live,
        drive: { samples, refused },
        released,
        renderer,
        opened,
        lost,
        reduced,
        reducedMotion,
        statuses: state.minecraftStatus,
        pins,
        sample: sample.current,
        waveFrozen: waveFrozen.current,
      }),
    [state, now, dpr, stage, narrow, touch, live, samples, refused, released, renderer, opened, lost, reduced, reducedMotion, pins],
  );
  viewRef.current = view;
  sample.current = view.field?.slots ?? [];
  const resetAt = state.accepted?.minecraft?.last_reset_all?.clicked_at ?? null;
  const kept = view.field?.wave?.frozen ?? null;
  if (waveFrozen.current !== null && waveFrozen.current.clicked_at !== resetAt) waveFrozen.current = null;
  if (kept !== null && waveFrozen.current === null) waveFrozen.current = kept;
  const cursorRef = useRef<number | null>(null);
  cursorRef.current = cursor;
  // The boxes in `view` were laid out in this stage: the 3D view draws into the same one.
  stageBox.current = stage;
  const panel = view.panel;

  // The sockets the view asks for.
  const wanted = JSON.stringify(view.cards.map((c) => ({ id: c.id, socket: c.socket, forkOf: c.forkOf })));
  useEffect(() => {
    tiles.sync(JSON.parse(wanted) as { id: string; socket: { url: string; first: string; key: string } | null; forkOf: string | null }[]);
  }, [tiles, wanted]);

  // One driver per borrow: a new session starts its own messages and measurements.
  if (panel === null) driverRef.current = null;
  else if (driverRef.current?.session !== panel.session) {
    driverRef.current = { session: panel.session, id: panel.id, driver: new Driver() };
  } else {
    // Tile ids are places in the layout: the borrowed server keeps its session when another
    // host's servers leave and its tile is renumbered.
    driverRef.current.id = panel.id;
  }
  useEffect(() => {
    setSamples([]);
    setRefused(null);
  }, [panel?.session]);

  // Send what the driver has due, then come back when it says.
  const pump = useCallback(() => {
    if (pumpTimer.current) clearTimeout(pumpTimer.current);
    pumpTimer.current = null;
    const d = driverRef.current;
    if (!d) return;
    const t = localNow();
    const msg = d.driver.tick(t);
    if (msg) tiles.send(d.id, msg);
    const next = d.driver.nextAt(localNow());
    if (next !== null) pumpTimer.current = setTimeout(pump, Math.max(0, next - localNow()));
  }, [tiles]);
  useEffect(() => () => {
    if (pumpTimer.current) clearTimeout(pumpTimer.current);
  }, []);

  // What the tiles say back: the driven bot's moves and the changed columns end a measurement.
  useEffect(() => {
    tiles.events = {
      status: (id, players, t) => {
        const d = driverRef.current;
        if (!d || d.id !== id) return;
        if (d.driver.observeStatus(players, t) !== null) setSamples([...d.driver.samples]);
      },
      delta: (id, t, columns) => {
        const d = driverRef.current;
        if (!d || d.id !== id) return;
        if (d.driver.observeDelta(t, columns) !== null) setSamples([...d.driver.samples]);
      },
      ack: (id, ack) => {
        const d = driverRef.current;
        if (!d || d.id !== id) return;
        d.driver.observeAck(ack.ok);
        setRefused(ack.ok ? null : (ack.reason ?? "The server refused it."));
      },
    };
    return () => {
      tiles.events = null;
    };
  }, [tiles]);

  const driving = panel?.driving === true;
  const giveBack = useCallback(() => {
    const p = viewRef.current?.panel;
    driverRef.current?.driver.releaseAll(localNow());
    pump();
    if (p) {
      setReleased(p.session);
      void client.act("release_minecraft", { claim: p.session });
    }
  }, [client, pump]);

  const input = useCallback(
    (inp: DriveInput, down: boolean) => {
      const d = driverRef.current;
      if (!d) return;
      const t = localNow();
      if (down) d.driver.press(inp, t);
      else d.driver.release(inp, t);
      pump();
    },
    [pump],
  );

  // Keys reach the bot only while this viewer holds the server and its socket acts as this viewer.
  useEffect(() => {
    if (panel === null) return;
    const onDown = (e: KeyboardEvent) => {
      if (e.code === "Escape") {
        giveBack();
        return;
      }
      if (!driving || e.metaKey || e.ctrlKey || e.altKey) return;
      const inp = driveInputOf(e.code);
      if (inp === null) return;
      e.preventDefault();
      if (e.repeat) return;
      input(inp, true);
    };
    const onUp = (e: KeyboardEvent) => {
      const inp = driveInputOf(e.code);
      if (inp === null || !driverRef.current) return;
      e.preventDefault();
      input(inp, false);
    };
    const onBlur = () => {
      driverRef.current?.driver.releaseAll(localNow());
      pump();
    };
    window.addEventListener("keydown", onDown);
    window.addEventListener("keyup", onUp);
    window.addEventListener("blur", onBlur);
    return () => {
      window.removeEventListener("keydown", onDown);
      window.removeEventListener("keyup", onUp);
      window.removeEventListener("blur", onBlur);
      onBlur();
    };
  }, [panel === null, driving, giveBack, input, pump]); // eslint-disable-line react-hooks/exhaustive-deps

  // A drag across the held tile turns the bot.
  const dragFrom = useRef<number | null>(null);
  const onPointerDown = (e: React.PointerEvent<HTMLCanvasElement>) => {
    if (!driving) return;
    capture(e.currentTarget, e.pointerId);
    dragFrom.current = e.clientX;
  };
  const onPointerMove = (e: React.PointerEvent<HTMLCanvasElement>) => {
    const d = driverRef.current;
    if (dragFrom.current === null || !d) return;
    const dx = e.clientX - dragFrom.current;
    dragFrom.current = e.clientX;
    d.driver.drag(dx, localNow());
    pump();
  };
  const onPointerUp = () => {
    dragFrom.current = null;
  };

  // An opened server that is no server any more is forgotten: its id used again later does not open by itself.
  useEffect(() => {
    if (view.openedGone) setOpened(null);
  }, [view.openedGone]);

  // Escape puts an opened server back in the wall (with a panel open, Escape gives the server back).
  useEffect(() => {
    if (opened === null || panel !== null) return;
    const onDown = (e: KeyboardEvent) => {
      if (e.code === "Escape") setOpened(null);
    };
    window.addEventListener("keydown", onDown);
    return () => window.removeEventListener("keydown", onDown);
  }, [opened, panel === null]); // eslint-disable-line react-hooks/exhaustive-deps

  // Focus follows: to "All servers" when a server opens, back to its tile when the wall returns.
  // A tile that cannot take it then (its server is restoring, lent, or gone: its picture is no
  // button) passes it to the first tile that can, and with none to this view's link in the mode
  // bar, so it is never left on nothing.
  const openedNow = view.opened === null ? null : opened;
  useEffect(() => {
    if (openedNow !== null) document.querySelector<HTMLElement>('[data-testid="mc-close"]')?.focus();
    else if (wasOpened.current !== null) {
      const own = canvases.current.get(wasOpened.current);
      const to =
        own && own.tabIndex >= 0
          ? own
          : (document.querySelector<HTMLElement>('canvas.mc-canvas[tabindex="0"]') ?? document.querySelector<HTMLElement>('[data-testid="mode-minecraft"]'));
      to?.focus();
    }
    wasOpened.current = openedNow;
  }, [openedNow]);

  // Draw: the 3D view every frame its guard allows; the flat map when a tile's world or players
  // changed, or its box moved.
  useEffect(() => {
    if (renderer === null) return;
    let id = 0;
    const frame = () => {
      const v = viewRef.current;
      const st = stage3d.current;
      if (v && renderer === "3d" && st) {
        const area = stageBox.current;
        if (area && document.visibilityState === "visible") {
          st.frame(localNow(), {
            stage: area,
            dpr,
            tiles: v.cards.map((c, index) => ({ id: c.id, slot: c.slot, index, box: c.box, forkOf: c.forkOf, flight: c.flight, timer: c.timer, dark: c.dark, mine: c.mine })),
            reducedMotion,
            labels: (tid) => canvases.current.get(tid),
            flights: flightsRef.current,
          });
        }
      }
      // The field: every server a cell, on its one canvas.
      const f = v?.field;
      if (f && f.layout && fieldCanvas.current) {
        fieldPainter.paint(fieldCanvas.current, { layout: f.layout, cells: f.cells, shown: f.shown, pinned: f.pinned, cursor: cursorRef.current, dpr, reducedMotion }, localNow());
      }
      if (v && renderer === "flat") {
        for (const c of v.cards) {
          const canvas = canvases.current.get(c.id);
          if (!canvas || !c.box) continue;
          const sig = `${c.box.x},${c.box.y},${c.box.w},${c.box.h},${dpr},${c.socket?.key ?? ""}`;
          const force = drawn.current.get(c.id) !== sig;
          if (tiles.draw(c.id, canvas, c.box, dpr, force)) drawn.current.set(c.id, sig);
        }
      }
      id = requestAnimationFrame(frame);
    };
    id = requestAnimationFrame(frame);
    return () => cancelAnimationFrame(id);
  }, [tiles, dpr, renderer, reducedMotion, fieldPainter]);

  // Test hook, only with ?debug=1.
  useEffect(() => {
    if (new URLSearchParams(window.location.search).get("debug") === "1") {
      window.__minecraft = {
        client,
        view: () => viewRef.current!,
        tiles,
        driver: () => {
          const d = driverRef.current?.driver;
          return d ? { samples: [...d.samples], yaw: d.yaw, keys: d.keys() } : null;
        },
        stage: () => stage3d.current?.debug() ?? null,
        setBudget: (on) => {
          if (stage3d.current) stage3d.current.budgetOn = on;
        },
        field: () => ({ paints: fieldPainter.paints, cells: fieldPainter.lastCells }),
      };
    }
  }, [client, tiles, fieldPainter]);

  // A cell picked from the field: its server takes a 3D tile, until it is unpinned or its minute is up.
  const fieldView = view.field;
  const pick = (i: number | null) => {
    const c = i === null ? undefined : fieldView?.cells[i];
    if (c) setPins((p) => [...p.filter((x) => x.n !== c.n), { n: c.n, at: localNow() }].slice(-6));
  };
  const unpin = (n: number) => setPins((p) => p.filter((x) => x.n !== n));
  const cellUnder = (e: React.PointerEvent<HTMLCanvasElement> | React.MouseEvent<HTMLCanvasElement>): number | null => {
    if (!fieldView?.layout) return null;
    const r = e.currentTarget.getBoundingClientRect();
    return cellAt(fieldView.layout, fieldView.cells.length, e.clientX - r.left, e.clientY - r.top);
  };
  /** Move the cursor by an arrow key or a step button, and keep its cell in view when the field scrolls. */
  const step = (key: "ArrowLeft" | "ArrowRight" | "ArrowUp" | "ArrowDown") => {
    if (!fieldView?.layout) return;
    const to = moveCursor(fieldView.layout, fieldView.cells.length, cursor, key);
    setCursor(to);
    const el = fieldScroll.current;
    if (to !== null && el) el.scrollTop = scrollTo(fieldView.layout, to, el.scrollTop, el.clientHeight);
  };
  const onFieldKey = (e: React.KeyboardEvent<HTMLCanvasElement>) => {
    if (e.key === "ArrowLeft" || e.key === "ArrowRight" || e.key === "ArrowUp" || e.key === "ArrowDown") {
      e.preventDefault();
      step(e.key);
    } else if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      pick(cursor);
    }
  };
  const pointed = cursor === null ? undefined : fieldView?.cells[cursor];
  const pointedPinned = pointed !== undefined && fieldView?.pinned.includes(pointed.n) === true;
  const fieldTouch = narrow || touch;

  const resetAllPending = state.pending.some((p) => p.intent.name === "minecraft_reset" && p.intent.args["all"] === true);
  const needsVerifyPrompt = view.verify.prompt;
  const sheetTop = stage ? Math.round(narrow ? stage.y + stage.h * 0.5 + 8 : stage.y) : 0;
  const h = view.header;

  return (
    <>
      <main className="win-page mc-page" data-testid="minecraft">
        <header className="win-top">
          <div className="win-top-row">
            <div className="masthead">
              <div className="brand">
                <span className="site">{SITE}</span>
                <h1>{view.title}</h1>
              </div>
              <ModeSwitch current="minecraft" />
            </div>
            <button type="button" className="btn" disabled={view.resetAll.disabled || resetAllPending} onClick={() => void client.act("minecraft_reset", { all: true })} data-testid="mc-reset-all">
              {view.resetAll.label}
            </button>
          </div>
          {h ? (
            <div className="win-stats" data-testid="mc-stats">
              <span data-testid="mc-up">{h.up}</span>
              {h.server ? <span data-testid="mc-server">{h.server}</span> : null}
              <span data-testid="mc-median">{h.median}</span>
              <span data-testid="mc-slowest">{h.slowest}</span>
              {h.lastFork ? <span data-testid="mc-last-fork">{h.lastFork}</span> : null}
              {h.lastResetAll ? <span data-testid="mc-last-reset-all">{h.lastResetAll}</span> : null}
            </div>
          ) : null}
          {h?.join ? <JoinLine join={h.join} /> : null}
          {h?.drawn ? (
            <p className="mc-drawn" data-testid="mc-drawn">
              {h.drawn}
            </p>
          ) : null}
        </header>
        <div ref={stageRef} className="win-stage" data-testid="mc-stage" />
      </main>
      {view.banner ? (
        <p className="win-banner" role="status" data-testid="mc-banner">
          {view.banner}
        </p>
      ) : null}
      {view.sleep ? (
        <div className="win-sleep">
          <WakeCard sleep={view.sleep} onWake={() => void client.act("wake")} />
        </div>
      ) : null}
      {view.empty ? (
        <p className="win-empty" role="status" data-testid="mc-empty">
          {view.empty}
        </p>
      ) : null}
      <div className="win-layer mc-layer" data-renderer={renderer ?? undefined} data-testid="mc-layer">
        {/* one WebGL canvas under every tile: each world is drawn into its tile's rectangle */}
        {renderer !== "flat" || lost ? <canvas ref={glRef} className="mc-gl" aria-hidden="true" style={stage && renderer !== "flat" ? at(stage) : { display: "none" }} data-testid="mc-gl" /> : null}
        {fieldView && fieldView.box && fieldView.grid && fieldView.layout ? (
          <section className="mc-field" style={at(fieldView.box)} aria-label="Every server" data-testid="mc-field">
            <div className="mc-field-text" style={{ height: narrow ? FIELD_TEXT_H_NARROW : FIELD_TEXT_H }}>
              <p className="servers" data-testid="mc-field-servers">
                {fieldView.headline.servers}
              </p>
              <p data-testid="mc-field-states">{fieldView.headline.states}</p>
              {/* a phone has room for three lines: the two readings share one, without their workings, and a reset's count takes its place while it runs */}
              {narrow ? (
                (fieldView.headline.perCpu || fieldView.headline.perGib) && !fieldView.wave ? (
                  <p data-testid="mc-field-density">
                    {[fieldView.headline.perCpu, fieldView.headline.perGib]
                      .filter((x) => x !== null)
                      .map((x) => x.replace(/ \(.*\)$/, ""))
                      .join(" · ")}
                  </p>
                ) : null
              ) : (
                <>
                  {fieldView.headline.perCpu ? <p data-testid="mc-field-density">{fieldView.headline.perCpu}</p> : null}
                  {fieldView.headline.perGib ? <p data-testid="mc-field-density-mem">{fieldView.headline.perGib}</p> : null}
                </>
              )}
              {fieldView.wave ? (
                <p className="wave" data-testid="mc-wave">
                  {fieldView.wave.text}
                </p>
              ) : fieldView.headline.tps && !narrow ? (
                <p data-testid="mc-field-tps">{fieldView.headline.tps}</p>
              ) : null}
            </div>
            {/* one canvas for every cell; it scrolls when the cells do not fit */}
            <div ref={fieldScroll} className="mc-field-scroll" style={{ maxHeight: fieldView.grid.h }} data-scrolls={fieldView.layout.scrolls ? "true" : undefined}>
              <canvas
                ref={fieldCanvas}
                className="mc-field-canvas"
                role="application"
                tabIndex={0}
                aria-label={fieldView.ariaLabel}
                aria-describedby="mc-cell-readout"
                style={{ width: fieldView.layout.w, height: fieldView.layout.h }}
                data-testid="mc-field-canvas"
                onPointerDown={(e) => {
                  lastPointer.current = e.pointerType;
                  setCursor(cellUnder(e));
                }}
                onPointerMove={(e) => setCursor(cellUnder(e))}
                // a finger leaves as it lifts: the cell it tapped stays the one described
                onPointerLeave={(e) => {
                  if (e.pointerType === "mouse") setCursor(null);
                }}
                onClick={(e) => {
                  if (lastPointer.current === "mouse") pick(cellUnder(e));
                }}
                onKeyDown={onFieldKey}
              />
            </div>
            {/* what the pointed cell is, a step to the next server either way, and the pick: the way in for a finger, which a 9 px cell is too small for */}
            <div className={`mc-field-readout${fieldTouch ? " touch" : ""}`} style={{ height: fieldTouch ? FIELD_READOUT_H_NARROW : FIELD_READOUT_H }}>
              <button type="button" className="btn mini" aria-label="Previous server" onClick={() => step("ArrowLeft")} data-testid="mc-cell-prev">
                ‹
              </button>
              <button type="button" className="btn mini" aria-label="Next server" onClick={() => step("ArrowRight")} data-testid="mc-cell-next">
                ›
              </button>
              <p id="mc-cell-readout" data-testid="mc-cell-readout" aria-live="polite">
                {pointed ? cellText(pointed, fieldTouch) : fieldTouch ? "Tap a server, or step to one." : "Pick a server to see it in 3D."}
              </p>
              {pointed && pointedPinned ? (
                <button type="button" className="btn mini" onClick={() => unpin(pointed.n)} data-testid="mc-cell-unpin">
                  Unpin
                </button>
              ) : (
                <button type="button" className="btn mini" disabled={!pointed} onClick={() => pick(cursor)} data-testid="mc-cell-show">
                  Show
                </button>
              )}
            </div>
            <p className="mc-field-legend" style={{ height: FIELD_LEGEND_H }} data-testid="mc-field-legend">
              {FIELD_LEGEND.map((l) => (
                <span key={l.key}>
                  <i style={l.rgb ? { background: `rgb(${l.rgb.join()})` } : undefined} className={l.rgb ? undefined : "none"} />
                  {l.text}
                </span>
              ))}
              <span>
                <i className="ring" />
                {FIELD_LEGEND_RING}
              </span>
              <span>{FIELD_LEGEND_YOUNG}</span>
            </p>
          </section>
        ) : null}
        {view.cards.map((c) =>
          c.box ? (
            <div
              key={c.id}
              className={`mc-tile${c.mine ? " mine" : ""}${renderer === "3d" ? " gl" : ""}${c.flight ? " taking" : ""}`}
              style={at(c.box)}
              data-testid={`mc-tile-${c.slot}`}
              data-phase={c.dark ? "dark" : "live"}
            >
              {/* the picture is the canvas: the stream message and its Retry stay outside it, in reach of a screen reader */}
              <canvas
                role={!c.dark && !c.mine && !c.opened ? "button" : "img"}
                tabIndex={!c.dark && !c.mine && !c.opened ? 0 : undefined}
                title={!c.dark && !c.mine && !c.opened ? `Open ${c.slot}` : undefined}
                aria-describedby={!c.dark && !c.mine && !c.opened ? `mc-desc-${c.slot}` : undefined}
                onClick={!c.dark && !c.mine && !c.opened ? () => setOpened(c.id) : undefined}
                onKeyDown={
                  !c.dark && !c.mine && !c.opened
                    ? (e) => {
                        if (e.key !== "Enter" && e.key !== " ") return;
                        e.preventDefault();
                        setOpened(c.id);
                      }
                    : undefined
                }
                aria-label={!c.dark && !c.mine && !c.opened ? `Open ${c.slot}` : c.ariaLabel}
                ref={(el) => {
                  if (el) canvases.current.set(c.id, el);
                  else canvases.current.delete(c.id);
                  drawn.current.delete(c.id);
                }}
                className="mc-canvas"
                data-testid={`mc-canvas-${c.slot}`}
                style={c.mine && driving ? { cursor: "ew-resize", touchAction: "none" } : undefined}
                onPointerDown={c.mine ? onPointerDown : undefined}
                onPointerMove={c.mine ? onPointerMove : undefined}
                onPointerUp={c.mine ? onPointerUp : undefined}
                onPointerCancel={c.mine ? onPointerUp : undefined}
              />
              {/* what the tile shows, for a screen reader: the canvas names its action when it is a button */}
              <span id={`mc-desc-${c.slot}`} className="sr-only" data-testid={`mc-desc-${c.slot}`}>
                {c.ariaLabel}
              </span>
              {/* a copy's count of the columns that differ from its source, in the colour they are outlined in */}
              {!c.dark && c.diff ? (
                <span className="diff" data-testid={`mc-diff-${c.slot}`}>
                  {c.diff}
                </span>
              ) : null}
              {!c.dark && c.stream ? (
                <span className={`stream${c.streamFailed ? " failed" : ""}`} role="status" data-testid={`mc-stream-${c.slot}`}>
                  <span className="text">{c.stream}</span>
                  {c.streamFailed && c.socket ? (
                    <button type="button" className="btn mini" onClick={() => tiles.retryNow(c.id)} data-testid={`mc-stream-retry-${c.slot}`}>
                      Retry
                    </button>
                  ) : null}
                </span>
              ) : null}
              {c.dark ? (
                <span className="dark" data-testid={`mc-dark-${c.slot}`}>
                  {c.timer ? (
                    <span className="timer" data-testid={`mc-timer-${c.slot}`}>
                      {c.timer}
                    </span>
                  ) : null}
                  {c.error ? (
                    <span className="error" data-testid={`mc-error-${c.slot}`}>
                      {c.error}
                    </span>
                  ) : null}
                </span>
              ) : null}
            </div>
          ) : null,
        )}
        {view.cards.map((c) => (
          <Caption
            key={c.id}
            c={c}
            onReset={() => void client.act("minecraft_reset", { slot: c.slot })}
            onFork={() => void client.act("minecraft_fork", { slot: c.slot })}
            onTake={() => void client.act("claim_minecraft", { slot: c.slot })}
            onClose={() => setOpened(null)}
            onUnpin={() => unpin(Number(c.slot.slice(3)))}
          />
        ))}
        {/* the frames of forks in flight (minecraft-stage.ts adds and moves them) */}
        <div ref={flightsRef} className="mc-flights" />
      </div>
      {panel ? <Sheet panel={panel} narrow={narrow} top={sheetTop} onGiveBack={giveBack} onPad={input} onRetry={() => tiles.retryNow(panel.id)} /> : null}
      {needsVerifyPrompt ? (
        <div className="verify-pop" role="dialog" aria-label={view.verify.title}>
          <VerifyCard client={client} verify={view.verify} />
        </div>
      ) : null}
      {view.notice ? (
        <p className="win-notice" role="alert" data-testid="mc-notice">
          {view.notice}
        </p>
      ) : null}
    </>
  );
}
