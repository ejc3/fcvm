"use client";

// Potato mode (`?mode=potato`): the ring of player VMs and the ball, the pace, the ladder, the
// last drop and the records. The page measures the ring's stage, ticks the clock once a frame and
// draws the view model core/potato.ts computes; every word on it comes from there.

import { useEffect, useLayoutEffect, useMemo, useRef, useState, useSyncExternalStore, type CSSProperties, type KeyboardEvent as ReactKeyboardEvent } from "react";
import { placeReadout, potatoColour, potatoView, SITE, type Box, type PotatoButton, type RingTile, type PotatoView } from "../../core/index.ts";
import { WallClient } from "./wall-client.ts";
import { VerifyCard } from "./Turnstile.tsx";
import { ModeSwitch } from "./ModeSwitch.tsx";
import { Landing, PotatoLegend } from "./Landing.tsx";
import { WakeCard } from "./Overlays.tsx";
import { useMedia } from "./hooks.ts";
import "./potato.css";

/** The legend's squares: one sample hue, since the legend says what a phase looks like and not whose it is. */
const SWATCH = potatoColour(8, 12);
/** One player's readout, placed from its measured size so it sits wholly inside the stage, on the side of its tile that keeps the ring and the ball's arc clear. */
function Readout({ tile, centre, stage }: { tile: RingTile | null; centre: { x: number; y: number }; stage: Box | null }) {
  const ref = useRef<HTMLDivElement | null>(null);
  const [at, setAt] = useState<{ x: number; y: number } | null>(null);
  const head = tile?.readout.head;
  const detail = tile?.readout.detail;
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el || !tile || !stage) return;
    setAt(placeReadout(tile.box, centre, { w: el.offsetWidth, h: el.offsetHeight }, stage));
  }, [tile?.box.x, tile?.box.y, tile?.box.w, head, detail, centre.x, centre.y, stage?.w, stage?.h]);
  if (tile === null || stage === null) return null;
  return (
    <div ref={ref} className="potato-readout" data-testid="potato-readout" aria-hidden="true" style={{ left: at?.x ?? 0, top: at?.y ?? 0, maxWidth: Math.max(80, stage.w - 8), visibility: at ? "visible" : "hidden" }}>
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

  // Room for the legend beside the ring: the page is in its two-column layout and the measured stage is not small.
  const twoColumns = useMedia("(min-width: 901px)");
  const legendOnPage = twoColumns && stage !== null && stage.w >= 360 && stage.h >= 360;
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
  const ring = view.ring;
  const lastPointer = useRef("mouse");
  // One tab stop for the ring: arrows, Home and End move between the tiles.
  const [roving, setRoving] = useState<number | null>(null);
  const rover = ring && ring.tiles.some((t) => t.player === roving) ? roving : (ring?.tiles[0]?.player ?? null);
  const onRingKey = (e: ReactKeyboardEvent<HTMLDivElement>) => {
    const els = [...e.currentTarget.querySelectorAll<HTMLElement>('[data-testid="potato-tile"]')];
    const i = els.indexOf(document.activeElement as HTMLElement);
    if (i < 0 || els.length === 0) return;
    const to = e.key === "ArrowRight" || e.key === "ArrowDown" ? (i + 1) % els.length : e.key === "ArrowLeft" || e.key === "ArrowUp" ? (i - 1 + els.length) % els.length : e.key === "Home" ? 0 : e.key === "End" ? els.length - 1 : -1;
    if (to < 0) return;
    e.preventDefault();
    els[to]!.focus();
  };
  // A card is shown for a tile that is still in the ring; Escape dismisses it; a timer does not outlive the page.
  const present = (n: number | null) => (n !== null && ring?.tiles.some((t) => t.player === n) ? n : null);
  const shown = present(pressed) ?? present(pointed) ?? present(focused);
  // A state that names a tile no longer in the ring is dropped, so the card does not come back when the ring does.
  useEffect(() => {
    if (ring === null) return;
    const gone = (n: number | null) => n !== null && !ring.tiles.some((t) => t.player === n);
    if (gone(pointed)) setPointed(null);
    if (gone(focused)) setFocused(null);
    if (gone(pressed)) setPressed(null);
  }, [ring, pointed, focused, pressed]);
  useEffect(() => {
    if (shown === null) return;
    const esc = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      setPointed(null);
      setFocused(null);
      setPressed(null);
    };
    window.addEventListener("keydown", esc);
    return () => window.removeEventListener("keydown", esc);
  }, [shown]);
  useEffect(() => () => window.clearTimeout(pressTimer.current), []);
  const needsVerifyPrompt = view.verify.prompt;

  return (
    <main className="potato-page" data-testid="potato">
      <header className="potato-top">
        <div className="masthead">
          <div className="brand">
            <span className="site">{SITE}</span>
            <h1>{view.title}</h1>
          </div>
          <Landing landing={view.landing} colour={SWATCH} legendInPanel={!legendOnPage} />
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
          <div ref={stageRef} className="potato-stage" data-testid="potato-stage" role="group" aria-label={ring?.ariaLabel ?? view.empty ?? ""} onKeyDown={onRingKey}>
            {ring && ring.geometry.radius > 0 ? (
              <span
                className="potato-track"
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
                    role="group"
                    aria-label={t.ariaLabel}
                    tabIndex={t.player === rover ? 0 : -1}
                    onPointerEnter={(e) => e.pointerType !== "touch" && setPointed(t.player)}
                    onPointerLeave={(e) => e.pointerType !== "touch" && setPointed((p) => (p === t.player ? null : p))}
                    onFocus={(e) => {
                      setRoving(t.player);
                      // keyboard focus only: a click or a tap focuses the tile without showing the card
                      if (e.currentTarget.matches(":focus-visible")) setFocused(t.player);
                    }}
                    onBlur={() => setFocused((p) => (p === t.player ? null : p))}
                    onPointerDown={(e) => {
                      lastPointer.current = e.pointerType;
                      if (e.pointerType === "touch") pressStart(t.player);
                    }}
                    onPointerUp={pressEnd}
                    onPointerCancel={pressEnd}
                    onContextMenu={(e) => lastPointer.current === "touch" && e.preventDefault()}
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
        </section>
        <div className="potato-side">
          {legendOnPage && view.landing.legend ? <PotatoLegend legend={view.landing.legend} form="strip" colour={SWATCH} /> : null}
          {view.drop ? (
            <section className="card potato-drop" role="status" aria-labelledby="potato-drop-h" data-testid="potato-drop">
              <h2 id="potato-drop-h">{view.drop.title}</h2>
              <p className="headline" data-testid="potato-drop-headline">
                {view.drop.headline}
              </p>
              {view.drop.listening ? <p data-testid="potato-drop-listening">{view.drop.listening}</p> : null}
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
          {view.pace ? (
            <section className="card potato-pace" aria-labelledby="potato-pace-h" data-testid="potato-pace">
              <div className="head">
                <h2 id="potato-pace-h">Hold</h2>
                <span className="hold" data-testid="potato-hold">
                  {view.pace.hold}
                </span>
              </div>
              <p className="where" data-testid="potato-where">
                {view.pace.where}
              </p>
              <div className={`meter${view.pace.meter.short ? " short" : ""}`} aria-hidden="true">
                <span className="fill" style={{ width: `${(view.pace.meter.turnaround * 100).toFixed(1)}%` }} />
                {view.pace.meter.need !== null ? <span className="need" style={{ left: `${(view.pace.meter.need * 100).toFixed(1)}%` }} /> : null}
              </div>
              <p>{view.pace.meter.turnaroundText}</p>
              {view.pace.meter.needText ? <p className="muted">{view.pace.meter.needText}</p> : null}
              {view.pace.pace ? (
                <p className="pace" data-testid="potato-pace-cap">
                  {view.pace.pace}
                </p>
              ) : null}
              <p className="muted">{view.pace.start}</p>
            </section>
          ) : null}
          {view.ladder ? (
            <section className="card potato-ladder" aria-labelledby="potato-ladder-h">
              <h2 id="potato-ladder-h">The ladder</h2>
              <table data-testid="potato-ladder">
                <caption>{view.ladder.caption}</caption>
                <thead>
                  <tr>
                    <th scope="col">{view.ladder.head.players}</th>
                    <th scope="col">{view.ladder.head.record}</th>
                    <th scope="col">{view.ladder.head.broke}</th>
                    <th scope="col">{view.ladder.head.rounds}</th>
                  </tr>
                </thead>
                <tbody>
                  {view.ladder.rows.map((r) => (
                    <tr key={r.size} aria-current={r.current ? "true" : undefined} data-skipped={r.skipped ? "true" : undefined} data-testid={`potato-rung-${r.size}`}>
                      <th scope="row">{r.players}</th>
                      <td data-testid="potato-rung-record">{r.record}</td>
                      <td data-testid="potato-rung-broke">{r.broke}</td>
                      <td>{r.rounds}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
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
