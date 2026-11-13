"use client";

// Minecraft mode (`?mode=minecraft`): every server's world drawn from above on a canvas of its
// own, from the tile's view socket, with the header's figures, Reset all, and per tile Reset,
// Fork and Take over. The server this viewer holds is driven from the keyboard (or the on-screen
// buttons of a touch layout) over that tile's socket. The page measures the stage, ticks a clock
// and draws the view model core/minecraft.ts computes; every word on it comes from there.

import { useCallback, useEffect, useMemo, useRef, useState, useSyncExternalStore, type CSSProperties } from "react";
import {
  driveInputOf,
  Driver,
  minecraftView,
  SITE,
  type Box,
  type DriveInput,
  type MinecraftCard,
  type MinecraftPanel,
  type MinecraftView,
} from "../../core/index.ts";
import { WallClient } from "./wall-client.ts";
import { McTiles } from "./minecraft-tiles.ts";
import { TurnstileWidget } from "./Turnstile.tsx";
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
    };
  }
}

function Verify({ client, view }: { client: WallClient; view: MinecraftView }) {
  const sitekey = client.getState().turnstile.sitekey;
  if (sitekey === null) return null;
  return (
    <div className="verify" data-testid="verify">
      <p>{view.verify.text}</p>
      <TurnstileWidget sitekey={sitekey} onToken={(token) => client.verify(token)} onError={(error) => client.dispatch({ type: "turnstile-error", error })} />
      {view.verify.error ? (
        <p className="error" role="alert">
          {view.verify.error}
        </p>
      ) : null}
    </div>
  );
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

function Caption({ c, onReset, onFork, onTake }: { c: MinecraftCard; onReset: () => void; onFork: () => void; onTake: () => void }) {
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
        </div>
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
      <span className="hint">Each server's own address is under its tile.</span>
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
  const client = useMemo(() => new WallClient(coordUrl, (st) => st.minecraft?.tiles ?? [], directUrl), [coordUrl, directUrl]);
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
    const id = setInterval(() => setNow(localNow()), 100);
    client.start();
    return () => {
      clearInterval(id);
      client.stop();
      tiles.stop();
    };
  }, [client, tiles]);

  // Hidden tabs do not count as viewers.
  useEffect(() => {
    const send = () => client.setVisible(document.visibilityState === "visible");
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
    () => minecraftView({ state, now: now || 0, dpr, stage, narrow, touch, live, drive: { samples, refused }, released }),
    [state, now, dpr, stage, narrow, touch, live, samples, refused, released],
  );
  viewRef.current = view;
  const panel = view.panel;

  // The sockets the view asks for.
  const wanted = JSON.stringify(view.cards.map((c) => ({ id: c.id, socket: c.socket })));
  useEffect(() => {
    tiles.sync(JSON.parse(wanted) as { id: string; socket: { url: string; first: string; key: string } | null }[]);
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

  // Draw every tile whose world or players changed, or whose box moved.
  useEffect(() => {
    let id = 0;
    const frame = () => {
      const v = viewRef.current;
      if (v) {
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
  }, [tiles, dpr]);

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
      };
    }
  }, [client, tiles]);

  const resetAllPending = state.pending.some((p) => p.intent.name === "minecraft_reset" && p.intent.args["all"] === true);
  const needsVerifyPrompt = view.verify.needed && state.turnstile.queued.length > 0;
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
      <div className="win-layer mc-layer">
        {view.cards.map((c) =>
          c.box ? (
            <div key={c.id} className={`mc-tile${c.mine ? " mine" : ""}`} style={at(c.box)} data-testid={`mc-tile-${c.slot}`} data-phase={c.dark ? "dark" : "live"} role="img" aria-label={c.ariaLabel}>
              <canvas
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
          />
        ))}
      </div>
      {panel ? <Sheet panel={panel} narrow={narrow} top={sheetTop} onGiveBack={giveBack} onPad={input} onRetry={() => tiles.retryNow(panel.id)} /> : null}
      {needsVerifyPrompt ? (
        <div className="verify-pop" role="dialog" aria-label={view.verify.title}>
          <Verify client={client} view={view} />
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
