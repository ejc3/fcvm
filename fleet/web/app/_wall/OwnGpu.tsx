"use client";

// The viewer's own GPU: a private VM restored for them, full screen, with its
// own camera, the time from their click to its first frame, and the minute
// before it goes back to the pool.

import { useEffect, useRef, type RefObject } from "react";
import { addDrag, CAMERA_KEYS, CAMERA_MOVES, emptyDrag, takeDrag, wheelDive, type DragAccumulator, type OwnView } from "../../core/index.ts";
import { Timeline } from "./FocusPanel.tsx";
import type { WallClient } from "./wall-client.ts";

const localNow = () => performance.timeOrigin + performance.now();

export function OwnGpu({ own, client, shotRef, onClose }: { own: OwnView; client: WallClient; shotRef: RefObject<HTMLDivElement | null>; onClose: () => void }) {
  const backRef = useRef<HTMLButtonElement | null>(null);
  const drag = useRef<{ id: number; x: number; y: number; acc: DragAccumulator } | null>(null);
  const wheel = useRef<DragAccumulator>(emptyDrag());
  const live = !own.ended && own.placed !== null;
  const liveRef = useRef(live);
  liveRef.current = live;

  useEffect(() => {
    backRef.current?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
      const i = CAMERA_KEYS[e.key];
      const m = i === undefined ? undefined : CAMERA_MOVES[i];
      if (!m || !liveRef.current || (e.target as HTMLElement | null)?.tagName === "INPUT") return;
      e.preventDefault();
      const r = takeDrag(addDrag(emptyDrag(), m.dx, m.dy, m.dive), localNow(), true);
      if (r.args) client.send("claim_camera", r.args);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose, client]);

  const send = (acc: DragAccumulator, force: boolean): DragAccumulator => {
    const r = takeDrag(acc, localNow(), force);
    if (r.args) client.send("claim_camera", r.args);
    return r.acc;
  };
  const nudge = (dx: number, dy: number, dive = 0) => {
    send(addDrag(emptyDrag(), dx, dy, dive), true);
  };
  return (
    <div className="own" role="dialog" aria-modal="true" aria-labelledby="own-title" data-testid="own">
      <nav>
        <button ref={backRef} type="button" className="btn" onClick={onClose} data-testid="own-back">
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
            <path d="M15 18l-6-6 6-6" />
          </svg>
          {own.back}
        </button>
        <h1 id="own-title">{own.title}</h1>
        <span className="status" data-testid="own-status" role="status">
          {own.status}
        </span>
      </nav>
      <div
        className={`shot${live ? " live" : ""}`}
        ref={shotRef}
        data-testid="own-shot"
        onPointerDown={(e) => {
          if (!live || e.button !== 0) return;
          (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
          drag.current = { id: e.pointerId, x: e.clientX, y: e.clientY, acc: emptyDrag() };
        }}
        onPointerMove={(e) => {
          const d = drag.current;
          const b = own.placed?.box;
          if (!d || d.id !== e.pointerId || !b) return;
          d.acc = addDrag(d.acc, (e.clientX - d.x) / b.w, (e.clientY - d.y) / b.h);
          d.x = e.clientX;
          d.y = e.clientY;
          d.acc = send(d.acc, false);
        }}
        onPointerUp={(e) => {
          const d = drag.current;
          if (!d || d.id !== e.pointerId) return;
          send(d.acc, true);
          drag.current = null;
        }}
        onPointerCancel={() => {
          drag.current = null;
        }}
        onWheel={(e) => {
          if (!live) return;
          wheel.current = send(addDrag(wheel.current, 0, 0, wheelDive(e.deltaY)), false);
        }}
      >
        <div className="clocks">
          <span className="plate timer" data-testid="own-timer">
            <span className="label">{own.timerLabel}</span>
            <span className={`value${own.timerRunning ? " running" : ""}`}>{own.timer}</span>
          </span>
          {own.countdownLabel ? (
            <span className="plate countdown-own" data-testid="own-countdown">
              <span className="label">{own.countdownLabel}</span>
              <span className="value">{own.countdown}</span>
            </span>
          ) : null}
        </div>
      </div>
      <footer>
        <div className="cam" role="group" aria-label={own.cameraLabel}>
          {own.moves.map((m) => (
            <button key={m.label} type="button" className="btn" disabled={!live} onClick={() => nudge(m.dx, m.dy, m.dive)} aria-label={m.label}>
              {m.symbol}
            </button>
          ))}
        </div>
        <p className="help">{own.help}</p>
        <p className="latency" data-testid="own-latency">{own.latency}</p>
        {own.steps.length > 0 ? (
          <div className="own-steps">
            <Timeline steps={own.steps} ec2={false} />
          </div>
        ) : null}
      </footer>
    </div>
  );
}
