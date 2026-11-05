"use client";

// Potato mode (`?mode=potato`): the ring of player VMs and the ball, the pace, the ladder, the
// last drop and the records. The page measures the ring's stage, ticks the clock once a frame and
// draws the view model core/potato.ts computes; every word on it comes from there.

import { useEffect, useMemo, useRef, useState, useSyncExternalStore, type CSSProperties } from "react";
import { potatoView, SITE, type Box, type PotatoButton, type PotatoView } from "../../core/index.ts";
import { WallClient } from "./wall-client.ts";
import { TurnstileWidget } from "./Turnstile.tsx";
import { ModeSwitch } from "./ModeSwitch.tsx";
import { WakeCard } from "./Overlays.tsx";
import { useMedia } from "./hooks.ts";
import "./potato.css";

const localNow = () => performance.timeOrigin + performance.now();

declare global {
  interface Window {
    /** With ?debug=1 the potato page exposes its client and its view, as the wall and the phones do. */
    __potato?: { client: WallClient; view: () => PotatoView };
  }
}

function Verify({ client, view }: { client: WallClient; view: PotatoView }) {
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

export function Potato({ coordUrl }: { coordUrl: string }) {
  // No frame streams: the game is drawn from the snapshot alone.
  const client = useMemo(() => new WallClient(coordUrl, () => []), [coordUrl]);
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

  const ring = view.ring;
  const needsVerifyPrompt = view.verify.needed && state.turnstile.queued.length > 0;

  return (
    <main className="potato-page" data-testid="potato">
      <header className="potato-top">
        <div className="masthead">
          <div className="brand">
            <span className="site">{SITE}</span>
            <h1>{view.title}</h1>
          </div>
          <ModeSwitch current="potato" />
        </div>
        <div className="potato-actions" role="group" aria-label="Game">
          <ActionButton b={view.faster} name="faster" onClick={() => void client.act("potato_faster")} />
          <ActionButton b={view.restart} name="restart" onClick={() => void client.act("potato_restart")} />
        </div>
      </header>
      <p className="potato-sentence">{view.sentence}</p>
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
                    title={t.ariaLabel}
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
          <Verify client={client} view={view} />
        </div>
      ) : null}
    </main>
  );
}
