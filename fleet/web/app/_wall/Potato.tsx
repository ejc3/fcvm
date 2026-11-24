"use client";

// Potato mode (`?mode=potato`): the ring of player VMs and the ball, the pace, the ladder, the
// last drop and the records. The page measures the ring's stage, ticks the clock once a frame and
// draws the view model core/potato.ts computes; every word on it comes from there.

import { useEffect, useMemo, useRef, useState, useSyncExternalStore, type CSSProperties } from "react";
import { potatoColour, potatoView, SITE, type Box, type PotatoButton, type RingTile, type PotatoView } from "../../core/index.ts";
import { WallClient } from "./wall-client.ts";
import { VerifyCard } from "./Turnstile.tsx";
import { ModeSwitch } from "./ModeSwitch.tsx";
import { Landing } from "./Landing.tsx";
import { ChartCard, CpuSlots, DensityCard, LadderBars } from "./PotatoCards.tsx";
import { WakeCard } from "./Overlays.tsx";
import { useMedia } from "./hooks.ts";
import "./potato.css";

/** The legend's squares: one sample hue, since the legend says what a phase looks like and not whose it is. */
const SWATCH = potatoColour(8, 12);
/** One player's readout, beside its tile on the side away from the ring's centre so the ring and the ball's arc stay clear, kept inside the stage. */
function Readout({ tile, centre, stage }: { tile: RingTile | null; centre: { x: number; y: number }; stage: Box | null }) {
  if (tile === null || stage === null) return null;
  const cx = tile.box.x + tile.box.w / 2;
  const cy = tile.box.y + tile.box.h / 2;
  const dx = cx - centre.x;
  const dy = cy - centre.y;
  const len = Math.hypot(dx, dy) || 1;
  const gap = Math.max(tile.box.w, tile.box.h) / 2 + 8;
  const x = Math.min(Math.max(cx + (dx / len) * gap, 8), stage.w - 8);
  const y = Math.min(Math.max(cy + (dy / len) * gap, 8), stage.h - 8);
  // Anchor the card's near corner or edge at that point: it grows away from the ring.
  const tx = Math.abs(dx / len) < 0.35 ? "-50%" : dx < 0 ? "-100%" : "0";
  const ty = Math.abs(dy / len) < 0.35 ? "-50%" : dy < 0 ? "-100%" : "0";
  return (
    <div className="potato-readout" data-testid="potato-readout" aria-hidden="true" style={{ left: x, top: y, transform: `translate(${tx}, ${ty})` }}>
      <span className="head">{tile.readout.head}</span>
      {tile.readout.detail ? <span className="detail">{tile.readout.detail}</span> : null}
    </div>
  );
}

const localNow = () => performance.timeOrigin + performance.now();

declare global {
  interface Window {
    /** With ?debug=1 the potato page exposes its client and its view, as the wall and the phones do. */
    __potato?: { client: WallClient; view: () => PotatoView };
  }
}


const at = (b: Box): CSSProperties => ({ left: b.x, top: b.y, width: b.w, height: b.h });

function ActionButton({ b, name, onClick }: { b: PotatoButton; name: string; onClick: () => void }) {
  return (
    <div className="potato-action">
      <button type="button" className="btn" disabled={b.disabled} onClick={onClick} data-testid={`potato-${name}`} aria-describedby={b.note ? `potato-${name}-note` : undefined}>
        {b.label}
      </button>
      {b.note ? (
        <span className="note" id={`potato-${name}-note`} data-testid={`potato-${name}-note`}>
          {b.note}
        </span>
      ) : null}
    </div>
  );
}

export function Potato({ coordUrl, directUrl }: { coordUrl: string; directUrl: string | null }) {
  // No frame streams: the game is drawn from the snapshot alone.
  const client = useMemo(() => new WallClient(coordUrl, () => [], directUrl), [coordUrl, directUrl]);
  const state = useSyncExternalStore(client.subscribe, client.getState, client.getState);
  const [now, setNow] = useState(0);
  const [stage, setStage] = useState<Box | null>(null);
  const reducedMotion = useMedia("(prefers-reduced-motion: reduce)");
  const stageRef = useRef<HTMLDivElement | null>(null);
  const viewRef = useRef<PotatoView | null>(null);

  useEffect(() => {
    document.title = "Hot potato";
  }, []);
  // The connection, and a clock that ticks every frame: the ball and the wake bars move with it.
  useEffect(() => {
    client.start();
    let id = 0;
    const tick = () => {
      setNow(localNow());
      id = requestAnimationFrame(tick);
    };
    id = requestAnimationFrame(tick);
    return () => {
      cancelAnimationFrame(id);
      client.stop();
    };
  }, [client]);
  // Hidden tabs do not count as viewers.
  useEffect(() => {
    const send = () => client.setVisible(document.visibilityState === "visible");
    send();
    document.addEventListener("visibilitychange", send);
    return () => document.removeEventListener("visibilitychange", send);
  }, [client]);
  // The ring's stage: its own size, with the tiles placed inside it.
  useEffect(() => {
    const el = stageRef.current;
    if (!el) return;
    const measure = () => {
      const w = el.clientWidth;
      const h = el.clientHeight;
      setStage((a) => (a && a.w === w && a.h === h ? a : w > 0 && h > 0 ? { x: 0, y: 0, w, h } : null));
    };
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const view = useMemo(() => potatoView({ state, now: now || localNow(), stage, reducedMotion }), [state, now, stage, reducedMotion]);
  viewRef.current = view;

  // Test hook, only with ?debug=1.
  useEffect(() => {
    if (new URLSearchParams(window.location.search).get("debug") === "1") {
      window.__potato = { client, view: () => viewRef.current! };
    }
  }, [client]);

  // The readout follows the pointer, the keyboard focus or a long press on touch; the text is re-read from the view, so it ticks.
  const [pointed, setPointed] = useState<number | null>(null);
  const [focused, setFocused] = useState<number | null>(null);
  const [pressed, setPressed] = useState<number | null>(null);
  const pressTimer = useRef(0);
  const pressStart = (player: number) => {
    window.clearTimeout(pressTimer.current);
    pressTimer.current = window.setTimeout(() => setPressed(player), 350);
  };
  const pressEnd = () => {
    window.clearTimeout(pressTimer.current);
    setPressed(null);
  };
  const shown = pressed ?? pointed ?? focused;
  const ring = view.ring;
  // The slots go in the ring's centre when the room inside the arc and its label is wide enough; else under the ring.
  const centreWidth = ring ? 2 * (ring.geometry.radius - ring.geometry.tile * 0.75 - 6 - 64) : 0;
  const slotsInRing = view.cpuSlots !== null && centreWidth >= 240;
  const needsVerifyPrompt = view.verify.prompt;

  return (
    <main className="potato-page" data-testid="potato">
      <header className="potato-top">
        <div className="masthead">
          <div className="brand">
            <span className="site">{SITE}</span>
            <h1>{view.title}</h1>
          </div>
          <Landing landing={view.landing} colour={SWATCH} />
          <ModeSwitch current="potato" />
        </div>
        <div className="potato-actions" role="group" aria-label="Game">
          <ActionButton b={view.faster} name="faster" onClick={() => void client.act("potato_faster")} />
          <ActionButton b={view.restart} name="restart" onClick={() => void client.act("potato_restart")} />
        </div>
      </header>
      {view.banner ? (
        <p className="potato-banner" role="status" data-testid="potato-banner">
          {view.banner}
        </p>
      ) : null}
      {view.notice ? (
        <p className="potato-notice" role="alert" data-testid="potato-notice">
          {view.notice}
        </p>
      ) : null}
      {view.sleep ? (
        <div className="potato-sleep">
          <WakeCard sleep={view.sleep} onWake={() => void client.act("wake")} />
        </div>
      ) : null}
      <div className="potato-body">
        <section className="potato-ring-wrap" aria-label="The ring">
          <p className="potato-caption" data-testid="potato-caption">
            {ring?.caption ?? view.empty ?? ""}
          </p>
          <div ref={stageRef} className="potato-stage" data-testid="potato-stage" role="img" aria-label={ring?.ariaLabel ?? view.empty ?? ""}>
            {ring && ring.geometry.radius > 0 ? (
              <span
                className={`potato-track${ring.flash ? " flash" : ""}`}
                aria-hidden="true"
                style={{ left: ring.geometry.centre.x - ring.geometry.radius, top: ring.geometry.centre.y - ring.geometry.radius, width: 2 * ring.geometry.radius, height: 2 * ring.geometry.radius }}
              />
            ) : null}
            {ring
              ? ring.tiles.map((t) => (
                  <div
                    key={t.player}
                    className="potato-tile"
                    data-testid="potato-tile"
                    data-player={t.player}
                    data-phase={t.phase}
                    data-holder={t.holder ? "true" : undefined}
                    data-late={t.late !== null ? "true" : undefined}
                    role="group"
                    aria-label={t.ariaLabel}
                    tabIndex={0}
                    onPointerEnter={(e) => e.pointerType !== "touch" && setPointed(t.player)}
                    onPointerLeave={(e) => e.pointerType !== "touch" && setPointed((p) => (p === t.player ? null : p))}
                    onFocus={() => setFocused(t.player)}
                    onBlur={() => setFocused((p) => (p === t.player ? null : p))}
                    onPointerDown={(e) => e.pointerType === "touch" && pressStart(t.player)}
                    onPointerUp={pressEnd}
                    onPointerCancel={pressEnd}
                    onContextMenu={(e) => e.preventDefault()}
                    style={{ ...at(t.box), ["--c" as string]: t.colour }}
                  >
                    {t.number !== null ? <span className="num">{t.number}</span> : null}
                    {t.wake ? (
                      <span className="wake-bar" aria-hidden="true">
                        <span style={{ width: `${(t.wake.fraction * 100).toFixed(1)}%` }} />
                      </span>
                    ) : null}
                  </div>
                ))
              : null}
            {ring
              ? ring.tiles.map((t) =>
                  t.line !== null ? (
                    <span key={`l${t.player}`} className="potato-line" data-phase={t.phase} style={{ left: t.box.x + t.box.w / 2, top: t.box.y + t.box.h + 4 }} aria-hidden="true">
                      {t.line}
                    </span>
                  ) : null,
                )
              : null}
            {ring?.arc ? (
              <>
                <svg className="potato-arc" width={stage?.w ?? 0} height={stage?.h ?? 0} aria-hidden="true">
                  <defs>
                    <marker id="potato-arrow" viewBox="0 0 10 10" refX="7" refY="5" markerWidth="7" markerHeight="7" orient="auto">
                      <path d="M0 0L10 5L0 10z" fill="currentColor" />
                    </marker>
                  </defs>
                  <path d={`M${ring.arc.from.x} ${ring.arc.from.y}A${ring.arc.radius} ${ring.arc.radius} 0 ${ring.arc.large ? 1 : 0} 1 ${ring.arc.to.x} ${ring.arc.to.y}`} markerEnd="url(#potato-arrow)" />
                </svg>
                <span className="potato-arc-label" data-testid="potato-arc-label" style={{ left: ring.arc.labelAt.x, top: ring.arc.labelAt.y }}>
                  {ring.arc.label}
                </span>
              </>
            ) : null}
            {ring
              ? ring.tiles.map((t) =>
                  t.late !== null ? (
                    <span key={`late${t.player}`} className="potato-late" data-testid="potato-late" style={{ left: t.box.x + t.box.w / 2, top: t.box.y + t.box.h + 4 }}>
                      {t.late}
                    </span>
                  ) : null,
                )
              : null}
            {slotsInRing && view.cpuSlots && ring ? (
              <div className="potato-centre" style={{ left: ring.geometry.centre.x, top: ring.geometry.centre.y, width: centreWidth }}>
                <CpuSlots slots={view.cpuSlots} />
              </div>
            ) : null}
            {shown !== null && ring ? <Readout tile={ring.tiles.find((t) => t.player === shown) ?? null} centre={ring.geometry.centre} stage={stage} /> : null}
            {ring?.ball ? (
              <div
                className="potato-ball"
                data-testid="potato-ball"
                data-flying={ring.ball.flying ? "true" : "false"}
                data-dropped={ring.ball.dropped ? "true" : "false"}
                style={{ left: ring.ball.at.x - ring.ball.size / 2, top: ring.ball.at.y - ring.ball.size / 2, width: ring.ball.size, height: ring.ball.size }}
                aria-hidden="true"
              />
            ) : null}
          </div>
          {!slotsInRing && view.cpuSlots ? (
            <div className="card potato-cpus-card">
              <CpuSlots slots={view.cpuSlots} />
            </div>
          ) : null}
        </section>
        <div className="potato-side">
          {view.drop ? (
            <section className="card potato-drop" role="status" aria-labelledby="potato-drop-h" data-testid="potato-drop">
              <h2 id="potato-drop-h">{view.drop.title}</h2>
              {view.drop.big ? (
                <p className="big-late" data-testid="potato-drop-big">
                  <span className="v">{view.drop.big.value}</span> {view.drop.big.word}
                </p>
              ) : null}
              <p className="headline" data-testid="potato-drop-headline">
                {view.drop.headline}
              </p>
              <p className="muted">{view.drop.context}</p>
              {view.drop.stages.map((x) => (
                <p key={x} className="muted">
                  {x}
                </p>
              ))}
              {view.drop.detail ? <p className="muted">{view.drop.detail}</p> : null}
              {view.drop.next ? (
                <p className="next" data-testid="potato-drop-next">
                  {view.drop.next}
                </p>
              ) : null}
            </section>
          ) : null}
          {view.density ? <DensityCard d={view.density} /> : null}
          {view.chart || view.ladder ? (
            <section className="card potato-ladder" aria-labelledby="potato-ladder-h">
              <h2 id="potato-ladder-h" className="sr-only">
                Pace and the ladder
              </h2>
              {view.chart ? <ChartCard c={view.chart} /> : null}
              {view.ladder ? <LadderBars l={view.ladder} /> : null}
            </section>
          ) : null}
          {view.records.length > 0 ? (
            <section className="card potato-records" aria-labelledby="potato-records-h">
              <h2 id="potato-records-h">Records</h2>
              <dl data-testid="potato-records">
                {view.records.map((r) => (
                  <div key={r.name} data-testid={`potato-record-${r.name}`}>
                    <dt>{r.label}</dt>
                    <dd>
                      <span className="v">{r.value}</span> <span className="muted">{r.note}</span>
                    </dd>
                  </div>
                ))}
              </dl>
            </section>
          ) : null}
        </div>
      </div>
      {needsVerifyPrompt ? (
        <div className="verify-pop" role="dialog" aria-label={view.verify.title}>
          <VerifyCard client={client} verify={view.verify} />
        </div>
      ) : null}
    </main>
  );
}
