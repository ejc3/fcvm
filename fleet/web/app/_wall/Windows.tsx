"use client";

// Windows mode (`?mode=windows`): the sandboxes' desktops, each drawn from its own stream on the
// canvas, with the header's figures, a Reset all button and, for the sandbox this viewer holds,
// the panel of fixed actions. Nothing here forwards a click, a key or text to a desktop: a click
// on a tile resets that sandbox. The page measures the stage, ticks a clock and draws the view
// model core/windows.ts computes; every word on it comes from there.

import { useEffect, useMemo, useRef, useState, useSyncExternalStore, type CSSProperties } from "react";
import {
  HIDDEN_PAUSE_MS,
  SITE,
  streamsSuspended,
  WINDOWS_ACTION_INTERVAL_MS,
  windowsView,
  type Box,
  type WindowCard,
  type WindowsPanel,
  type WindowsView,
} from "../../core/index.ts";
import { canDecodeH264, WallClient } from "./wall-client.ts";
import { TurnstileWidget } from "./Turnstile.tsx";
import { ModeSwitch } from "./ModeSwitch.tsx";
import { WakeCard } from "./Overlays.tsx";
import { boxOf, sameBox, useMedia } from "./hooks.ts";
import "./windows.css";

const localNow = () => performance.timeOrigin + performance.now();

declare global {
  interface Window {
    /** With ?debug=1 the Windows page exposes the same hook as the wall, with its own view. */
    __windows?: { client: WallClient; view: () => WindowsView };
  }
}

function Verify({ client, view }: { client: WallClient; view: WindowsView }) {
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

function Caption({ c, onTake }: { c: WindowCard; onTake: () => void }) {
  if (!c.caption) return null;
  return (
    <div className={`win-caption${c.compact ? " compact" : ""}`} style={at(c.caption)} data-testid={`sandbox-caption-${c.slot}`} data-mine={c.mine ? "true" : undefined} data-lent={c.lent ? "true" : undefined}>
      <div className="row">
        <strong className="slot" data-testid={`sandbox-slot-${c.slot}`}>
          {c.slot}
        </strong>
        {c.desktopIn ? (
          <span className="in" data-testid={`sandbox-desktop-in-${c.slot}`}>
            {c.desktopIn}
          </span>
        ) : null}
        {c.showTake ? (
          <button type="button" className="btn take" disabled={c.take.disabled} title={c.take.title ?? undefined} onClick={onTake} data-testid={`sandbox-take-${c.slot}`}>
            {c.take.label}
          </button>
        ) : null}
      </div>
      {!c.compact && c.edition ? (
        <p className="edition" title={c.edition} data-testid={`sandbox-edition-${c.slot}`}>
          {c.edition}
        </p>
      ) : null}
      {/* On every tile in every layout: the evaluation's terms ask for it. */}
      <p className="label" data-testid={`sandbox-label-${c.slot}`}>
        {c.label}
      </p>
      {!c.compact ? (
        <>
          {c.lentText ? (
            <p className="lent" data-testid={`sandbox-lent-${c.slot}`}>
              {c.lentText}
            </p>
          ) : null}
        </>
      ) : null}
    </div>
  );
}

function Sheet({ panel, narrow, top, onGiveBack, onAction }: { panel: WindowsPanel; narrow: boolean; top: number; onGiveBack: () => void; onAction: (action: string, claim: string) => void }) {
  return (
    <aside className={`win-sheet${narrow ? " narrow" : ""}`} role="dialog" aria-labelledby="win-sheet-title" data-testid="win-panel" style={top > 0 ? { top } : undefined}>
      <div className="win-sheet-body">
        <section className="card">
          <div className="head">
            <h2 id="win-sheet-title" data-testid="win-panel-title">
              {panel.title}
            </h2>
            <span className="total" data-testid="win-countdown">
              {panel.countdown}
            </span>
          </div>
          <p className="note" data-testid="win-panel-note">
            {panel.note}
          </p>
          <div className="actions-grid" role="group" aria-label="Actions" data-testid="win-actions">
            {panel.actions.map((a) => (
              <button key={a.name} type="button" className="btn" disabled={a.disabled} onClick={() => onAction(a.name, panel.session)} data-testid={`win-action-${a.name}`}>
                {a.label}
              </button>
            ))}
          </div>
          <button type="button" className="btn" style={{ justifyContent: "center" }} onClick={onGiveBack} data-testid="win-give-back">
            {panel.giveBack}
          </button>
          <p className="label" data-testid="win-panel-label">
            {panel.label}
          </p>
        </section>
      </div>
    </aside>
  );
}

export function Windows({ coordUrl, directUrl }: { coordUrl: string; directUrl: string | null }) {
  const client = useMemo(() => new WallClient(coordUrl, (st) => st.windows?.tiles ?? [], directUrl), [coordUrl, directUrl]);
  const state = useSyncExternalStore(client.subscribe, client.getState, client.getState);
  const [now, setNow] = useState(0);
  const [canDecode, setCanDecode] = useState(true);
  const [painted, setPainted] = useState(false);
  const [dpr, setDpr] = useState(1);
  const [viewport, setViewport] = useState({ w: 0, h: 0 });
  const [stage, setStage] = useState<Box | null>(null);
  const narrow = useMedia("(max-width: 700px)");
  const reducedMotion = useMedia("(prefers-reduced-motion: reduce)");
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const stageRef = useRef<HTMLDivElement | null>(null);
  const viewRef = useRef<WindowsView | null>(null);

  useEffect(() => {
    document.title = "Windows";
  }, []);

  // Clock, capability, connection. The tiles' timers count tenths of a second.
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

  // Hidden tabs do not count as viewers, and after HIDDEN_PAUSE_MS they close their frame streams.
  useEffect(() => {
    const send = () => client.setVisible(document.visibilityState === "visible");
    send();
    let timer: ReturnType<typeof setInterval> | null = null;
    const check = () => {
      const ui = client.getState().ui;
      const sus = streamsSuspended(ui.hiddenSince, localNow(), HIDDEN_PAUSE_MS);
      if (sus !== ui.suspended) client.ui({ suspended: sus });
    };
    const onVisibility = () => {
      send();
      if (document.visibilityState === "hidden") {
        if (client.getState().ui.hiddenSince === null) client.ui({ hiddenSince: localNow() });
        timer ??= setInterval(check, 10_000);
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

  // Screen size and the stage.
  useEffect(() => {
    const measure = () => {
      setDpr(window.devicePixelRatio || 1);
      setViewport((v) => (v.w === window.innerWidth && v.h === window.innerHeight ? v : { w: window.innerWidth, h: window.innerHeight }));
      setStage((a) => {
        const next = boxOf(stageRef.current);
        return sameBox(a, next) ? a : next;
      });
    };
    measure();
    const ro = new ResizeObserver(measure);
    if (stageRef.current) ro.observe(stageRef.current);
    window.addEventListener("resize", measure);
    const mq = window.matchMedia(`(resolution: ${window.devicePixelRatio}dppx)`);
    mq.addEventListener("change", measure);
    return () => {
      ro.disconnect();
      window.removeEventListener("resize", measure);
      mq.removeEventListener("change", measure);
    };
  }, [narrow]);

  const view = useMemo(
    () => windowsView({ state, now: now || 0, dpr, stage, narrow, reducedMotion, canDecode }),
    [state, now, dpr, stage, narrow, reducedMotion, canDecode],
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
    client.plan(view.plan, { background: "#000000", dimAlpha: 0.5, fadeInMs: 280 });
  }, [client, view.plan]);

  // Test hook, only with ?debug=1.
  useEffect(() => {
    if (new URLSearchParams(window.location.search).get("debug") === "1") {
      window.__windows = { client, view: () => viewRef.current! };
    }
  }, [client]);

  // The coordinator takes one action a second from a holder: the page spaces its own, so a quick
  // second click waits its turn rather than being refused.
  const nextActionAt = useRef(0);
  const act = useMemo(
    () => (action: string, claim: string) => {
      const t = localNow();
      const at = Math.max(t, nextActionAt.current);
      nextActionAt.current = at + WINDOWS_ACTION_INTERVAL_MS + 100;
      const send = () => client.act("windows_action", { action, claim });
      if (at > t) setTimeout(send, at - t);
      else send();
    },
    [client],
  );

  const resetAllPending = state.pending.some((p) => p.intent.name === "windows_reset" && p.intent.args["all"] === true);
  const needsVerifyPrompt = view.verify.needed && state.turnstile.queued.length > 0;
  const sheetTop = stage ? Math.round(narrow ? stage.y + stage.h * 0.5 + 8 : stage.y) : 0;
  const h = view.header;

  return (
    <>
      <canvas ref={canvasRef} className={`frames${painted ? " painted" : ""}`} aria-hidden="true" />
      <main className="win-page" data-testid="windows">
        <header className="win-top">
          <div className="win-top-row">
            <div className="masthead">
              <div className="brand">
                <span className="site">{SITE}</span>
                <h1>{view.title}</h1>
              </div>
              <ModeSwitch current="windows" />
            </div>
            <button type="button" className="btn" disabled={view.resetAll.disabled || resetAllPending} onClick={() => void client.act("windows_reset", { all: true })} data-testid="win-reset-all">
              {view.resetAll.label}
            </button>
          </div>
          {h ? (
            <div className="win-stats" data-testid="win-stats">
              <span data-testid="win-up">{h.up}</span>
              <span data-testid="win-median">{h.median}</span>
              <span data-testid="win-slowest">{h.slowest}</span>
              <span data-testid="win-last-reset-all">{h.lastResetAll}</span>
              {h.image ? <span data-testid="win-image">{h.image}</span> : null}
            </div>
          ) : null}
          {h?.warning ? (
            <p className="win-warning" role="status" data-testid="win-warning">
              {h.warning}
            </p>
          ) : null}
          {h?.refused ? (
            <p className="win-refused" role="alert" data-testid="win-refused">
              {h.refused}
            </p>
          ) : null}
          {h?.gold ? (
            <p className="win-gold" role="status" data-testid="win-gold">
              {h.gold}
            </p>
          ) : null}
        </header>
        <div ref={stageRef} className="win-stage" data-testid="win-stage" />
      </main>
      {view.banner ? (
        <p className="win-banner" role="status" data-testid="win-banner">
          {view.banner}
        </p>
      ) : null}
      {view.sleep ? (
        <div className="win-sleep">
          <WakeCard sleep={view.sleep} onWake={() => void client.act("wake")} />
        </div>
      ) : null}
      {view.empty ? (
        <p className="win-empty" role="status" data-testid="win-empty">
          {view.empty}
        </p>
      ) : null}
      <div className="win-layer">
        {view.cards.map((c) =>
          c.box ? (
            <button
              key={c.id}
              type="button"
              className="win-hit"
              style={at(c.box)}
              disabled={c.reset.disabled}
              aria-label={c.ariaLabel}
              data-testid={`sandbox-hit-${c.slot}`}
              data-phase={c.dark ? "dark" : "live"}
              onClick={() => void client.act("windows_reset", { slot: c.slot })}
            >
              {c.dark ? (
                <span className="dark" data-testid={`sandbox-dark-${c.slot}`}>
                  {c.timer ? (
                    <span className="timer" data-testid={`sandbox-timer-${c.slot}`}>
                      {c.timer}
                    </span>
                  ) : null}
                  {c.error ? (
                    <span className="error" data-testid={`sandbox-error-${c.slot}`}>
                      {c.error}
                    </span>
                  ) : null}
                </span>
              ) : null}
            </button>
          ) : null,
        )}
        {view.cards.map((c) => (
          <Caption key={c.id} c={c} onTake={() => void client.act("claim_windows", { slot: c.slot })} />
        ))}
      </div>
      {view.panel ? <Sheet panel={view.panel} narrow={narrow} top={sheetTop} onGiveBack={() => void client.act("release_windows", { claim: view.panel!.session })} onAction={act} /> : null}
      {needsVerifyPrompt ? (
        <div className="verify-pop" role="dialog" aria-label={view.verify.title}>
          <Verify client={client} view={view} />
        </div>
      ) : null}
      {view.notice ? (
        <p className="win-notice" role="alert" data-testid="win-notice">
          {view.notice}
        </p>
      ) : null}
    </>
  );
}
