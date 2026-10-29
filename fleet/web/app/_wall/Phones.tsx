"use client";

// Phones mode (`?mode=phones`): the phones' own image on the canvas, seamless until the fleet
// zooms out, then each phone apart in a body of its own. The zoom is shared by every viewer
// (state.phones.zoomed_out); which phone is opened is this viewer's own. The page measures the
// stage, forwards input as actions and draws the view model core/phones.ts computes.

import { useEffect, useMemo, useRef, useState, useSyncExternalStore, type CSSProperties } from "react";
import { HIDDEN_PAUSE_MS, SITE, streamsSuspended, zoomProgressAt, phonesView, type Box, type PhoneCard, type PhonePanel, type PhonesView } from "../../core/index.ts";
import { canDecodeH264, WallClient } from "./wall-client.ts";
import { TurnstileWidget } from "./Turnstile.tsx";
import { Timeline } from "./FocusPanel.tsx";
import { boxOf, sameBox, useMedia } from "./hooks.ts";
import "./phones.css";

const localNow = () => performance.timeOrigin + performance.now();

declare global {
  interface Window {
    /** With ?debug=1 the phones page exposes the same hook as the wall, with its own view. */
    __phones?: { client: WallClient; view: () => PhonesView };
  }
}

function Verify({ client, view }: { client: WallClient; view: PhonesView }) {
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

function Bezel({ card }: { card: PhoneCard }) {
  if (!card.bezel || card.bezelOpacity <= 0.001) return null;
  const p = card.pad;
  return (
    <div
      className="phone-body"
      data-testid="phone-bezel"
      data-phone={card.id}
      aria-hidden="true"
      style={{
        ...at(card.bezel),
        borderWidth: `${p.top}px ${p.right}px ${p.bottom}px ${p.left}px`,
        borderRadius: card.radius,
        opacity: card.bezelOpacity,
        ["--pt" as string]: `${p.top}px`,
      }}
    />
  );
}

function PanelView({
  panel,
  narrow,
  top,
  client,
  slot,
  onBack,
  backRef,
}: {
  panel: PhonePanel;
  narrow: boolean;
  top: number;
  client: WallClient;
  slot: string | null;
  onBack: () => void;
  backRef: React.RefObject<HTMLButtonElement | null>;
}) {
  const b = panel.borrow;
  return (
    <aside className={`phone-sheet${narrow ? " narrow" : ""}`} role="dialog" aria-labelledby="phone-title" data-testid="phone-panel" style={narrow ? { top } : undefined}>
      <div className="phone-sheet-head">
        <button ref={backRef} type="button" className="btn" onClick={onBack} data-testid="phone-back">
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
            <path d="M15 18l-6-6 6-6" />
          </svg>
          {panel.back}
        </button>
      </div>
      <div className="phone-sheet-body">
        <section className="card">
          <div className="head">
            <h1 id="phone-title">{panel.title}</h1>
            <span className="state" data-testid="phone-state">
              {panel.state}
            </span>
          </div>
          <dl data-testid="phone-rows">
            {panel.rows.map((r) => (
              <div key={r.term} style={{ display: "contents" }}>
                <dt>{r.term}</dt>
                <dd>{r.value}</dd>
              </div>
            ))}
          </dl>
        </section>
        <section className="card" aria-labelledby="borrow-h" data-testid="phone-borrow" data-kind={b.kind}>
          {b.kind === "free" ? (
            <>
              <h2 id="borrow-h">Take over</h2>
              <p className="note">{b.note}</p>
              <button type="button" className="btn primary" style={{ justifyContent: "center" }} disabled={b.button.disabled} onClick={() => client.act("claim_phone", slot === null ? {} : { phone: slot })} data-testid="phone-take">
                {b.button.label}
              </button>
            </>
          ) : b.kind === "yours" ? (
            <>
              <div className="head">
                <h2 id="borrow-h">It is yours</h2>
                <span className="total" data-testid="phone-countdown">
                  {b.countdown}
                </span>
              </div>
              <div className="actions-grid" role="group" aria-label="Actions">
                {b.actions.map((a) => (
                  <button key={a.name} type="button" className="btn" disabled={a.disabled} onClick={() => client.act("phone_action", { action: a.name, claim: b.session })} data-testid={`phone-action-${a.name}`}>
                    {a.label}
                  </button>
                ))}
              </div>
              <button type="button" className="btn" style={{ justifyContent: "center" }} onClick={() => client.act("release_phone", { claim: b.session })} data-testid="phone-give-back">
                {b.giveBack}
              </button>
            </>
          ) : (
            <>
              <h2 id="borrow-h">Borrowed</h2>
              <p className="note" data-testid="phone-lent">
                {b.text}
              </p>
            </>
          )}
        </section>
        <section className="card" aria-labelledby="restore-h">
          <div className="head">
            <h2 id="restore-h">{panel.restoreTitle}</h2>
            <span className="total" data-testid="phone-restore-total">
              {panel.restoreTotal}
            </span>
          </div>
          <p className="note">{panel.restoreNote}</p>
          {panel.restoreNow ? <p className="now">{panel.restoreNow}</p> : null}
          <Timeline steps={panel.timeline} ec2={false} />
        </section>
      </div>
    </aside>
  );
}

export function Phones({ coordUrl }: { coordUrl: string; mediaUrl: string }) {
  const client = useMemo(() => new WallClient(coordUrl, (st) => st.phones?.tiles ?? []), [coordUrl]);
  const state = useSyncExternalStore(client.subscribe, client.getState, client.getState);
  const [now, setNow] = useState(0);
  const [canDecode, setCanDecode] = useState(true);
  const [painted, setPainted] = useState(false);
  const [dpr, setDpr] = useState(1);
  const [viewport, setViewport] = useState({ w: 0, h: 0 });
  const [stage, setStage] = useState<Box | null>(null);
  const [openId, setOpenId] = useState<string | null>(null);
  const narrow = useMedia("(max-width: 700px)");
  const reducedMotion = useMedia("(prefers-reduced-motion: reduce)");
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const stageRef = useRef<HTMLDivElement | null>(null);
  const viewRef = useRef<PhonesView | null>(null);
  const backRef = useRef<HTMLButtonElement | null>(null);
  const cardButtons = useRef(new Map<string, HTMLButtonElement>());
  const returnTo = useRef<string | null>(null);

  useEffect(() => {
    document.title = "Phones";
  }, []);

  // Clock, capability, connection.
  useEffect(() => {
    setNow(localNow());
    const id = setInterval(() => setNow(localNow()), 250);
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

  // The zoom is the fleet's: it moves toward state.phones.zoomed_out over ZOOM_MS from the moment
  // the snapshot flips. The first snapshot sets it without a run, and so does reduced motion.
  const phones = state.accepted?.phones ?? null;
  const target = phones?.zoomed_out ? 1 : 0;
  const hasPhones = phones !== null;
  const [progress, setProgress] = useState(0);
  const progressRef = useRef(0);
  const settled = useRef<number | null>(null);
  useEffect(() => {
    if (!hasPhones) return;
    const set = (p: number) => {
      progressRef.current = p;
      setProgress(p);
    };
    if (settled.current === null || reducedMotion) {
      settled.current = target;
      set(target);
      return;
    }
    if (settled.current === target) return;
    settled.current = target;
    const from = progressRef.current;
    const start = performance.now();
    let id = 0;
    const tick = () => {
      const p = zoomProgressAt(from, target, start, performance.now());
      set(p);
      if (p !== target) id = requestAnimationFrame(tick);
    };
    id = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(id);
  }, [hasPhones, target, reducedMotion]);

  const view = useMemo(
    () => phonesView({ state, now: now || 0, dpr, stage, progress, narrow, reducedMotion, openId, canDecode }),
    [state, now, dpr, stage, progress, narrow, reducedMotion, openId, canDecode],
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
      window.__phones = { client, view: () => viewRef.current! };
    }
  }, [client]);

  // ---- opening a phone -------------------------------------------------------------

  const panelOpen = view.panel !== null;
  const open = (id: string) => {
    returnTo.current = id;
    setOpenId(id);
  };
  const close = () => {
    setOpenId(null);
    const id = returnTo.current;
    returnTo.current = null;
    requestAnimationFrame(() => (id ? cardButtons.current.get(id) : null)?.focus());
  };
  useEffect(() => {
    if (panelOpen) backRef.current?.focus();
  }, [panelOpen]);
  useEffect(() => {
    if (!panelOpen) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        setOpenId(null);
        const id = returnTo.current;
        returnTo.current = null;
        requestAnimationFrame(() => (id ? cardButtons.current.get(id) : null)?.focus());
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [panelOpen]);

  const zoomPending = state.pending.some((p) => p.intent.name === "phones_zoom");
  const needsVerifyPrompt = view.verify.needed && state.turnstile.queued.length > 0;
  const sheetTop = stage ? Math.round(stage.y + stage.h * 0.5 + 8) : 0;
  const openedSlot = openId === null ? null : (phones?.tiles.find((t) => t.id === openId)?.slot ?? null);

  return (
    <>
      <canvas ref={canvasRef} className={`frames${painted ? " painted" : ""}`} aria-hidden="true" />
      <main className="phones-page" data-testid="phones" data-progress={view.layout?.eased.toFixed(3) ?? ""}>
        <header className="phones-top">
          <div className="brand">
            <span className="site">{SITE}</span>
            <h1>{view.title}</h1>
          </div>
          {!panelOpen ? (
            <button
              type="button"
              className="btn"
              disabled={view.zoom.disabled || zoomPending}
              aria-pressed={view.zoom.out}
              onClick={() => client.act("phones_zoom", { out: !view.zoom.out })}
              data-testid="phones-zoom"
            >
              {view.zoom.label}
            </button>
          ) : null}
        </header>
        <div ref={stageRef} className="phones-stage" data-testid="phones-stage" />
      </main>
      {view.banner ? (
        <p className="phones-banner" role="status" data-testid="phones-banner">
          {view.banner}
        </p>
      ) : null}
      {view.empty ? (
        <p className="phones-empty" role="status" data-testid="phones-empty">
          {view.empty}
        </p>
      ) : null}
      <div className="phones-layer">
        {view.cards.map((c) => (
          <Bezel key={c.id} card={c} />
        ))}
        {!panelOpen
          ? view.cards.map((c) =>
              c.bezel ? (
                <button
                  key={c.id}
                  ref={(el) => {
                    if (el) cardButtons.current.set(c.id, el);
                    else cardButtons.current.delete(c.id);
                  }}
                  type="button"
                  className="phone-hit"
                  style={at(c.bezel)}
                  aria-label={c.ariaLabel}
                  data-testid={`phone-hit-${c.id}`}
                  onClick={() => open(c.id)}
                />
              ) : null,
            )
          : null}
        {view.cards.map((c) =>
          c.labelBox && c.bezelOpacity > 0.001 ? (
            <div key={c.id} className="phone-label" style={{ ...at(c.labelBox), opacity: c.bezelOpacity }} data-testid={`phone-label-${c.id}`}>
              <span>{c.label}</span>
            </div>
          ) : null,
        )}
      </div>
      {view.panel ? <PanelView panel={view.panel} narrow={narrow} top={sheetTop} client={client} slot={openedSlot} onBack={close} backRef={backRef} /> : null}
      {needsVerifyPrompt ? (
        <div className="verify-pop" role="dialog" aria-label={view.verify.title}>
          <Verify client={client} view={view} />
        </div>
      ) : null}
      {view.notice ? (
        <p className="phones-notice" role="alert" data-testid="phone-notice">
          {view.notice}
        </p>
      ) : null}
    </>
  );
}
