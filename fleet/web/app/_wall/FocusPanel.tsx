"use client";

// One VM opened: its own stream at the renderer's native size, the timeline of
// its last restart, and its GPU's live readings. The canvas draws the stream
// into the measured shot box; this panel draws everything around it.

import { useEffect, useRef, type CSSProperties, type RefObject } from "react";
import type { FocusView, TimelineStep } from "../../core/index.ts";

const BLUES = ["#BFE3FF", "#7CC4FF", "#3E95D6", "#2A6FA3", "#1B4A6E", "#123552"];
const ORANGES = ["#FFD089", "#F5A524", "#B8780F", "#7A4F08", "#5A3A06", "#3F2904"];

export function Timeline({ steps, ec2 }: { steps: TimelineStep[]; ec2: boolean }) {
  const shades = ec2 ? ORANGES : BLUES;
  return (
    <>
      <span className="phase-bar" aria-hidden="true">
        {steps.map((s, i) => (
          <span
            key={i}
            className={s.hatch ? "hatch" : undefined}
            title={s.label}
            style={{ width: `${(s.fraction * 100).toFixed(2)}%`, background: s.hatch ? undefined : shades[s.shade % 6] }}
          />
        ))}
      </span>
      <ol className="legend" data-testid="timeline">
        {steps.map((s, i) => (
          <li key={i}>
            <span>
              <span className={`sw${s.hatch ? " hatch" : ""}`} style={s.hatch ? undefined : { background: shades[s.shade % 6] }} aria-hidden="true" />
              {s.label}
            </span>
            <span className="v">{s.value}</span>
          </li>
        ))}
      </ol>
    </>
  );
}

export function FocusPanel({
  focus,
  shotRef,
  onClose,
  onRestart,
}: {
  focus: FocusView;
  shotRef: RefObject<HTMLDivElement | null>;
  onClose: () => void;
  onRestart: () => void;
}) {
  const backRef = useRef<HTMLButtonElement | null>(null);
  useEffect(() => {
    backRef.current?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);
  const accent = { ["--accent" as string]: focus.accent } as CSSProperties;
  const ec2 = focus.id.startsWith("ec2");
  return (
    <div className="focus" role="dialog" aria-modal="true" aria-labelledby="focus-title" data-testid="focus" style={accent}>
      <nav>
        <button ref={backRef} type="button" className="btn" onClick={onClose}>
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
            <path d="M15 18l-6-6 6-6" />
          </svg>
          {focus.back}
        </button>
        <h1 id="focus-title">{focus.title}</h1>
        <span className="state" data-testid="focus-state">{focus.state}</span>
      </nav>
      <div className="body">
        <figure>
          <div className="shot" ref={shotRef} data-testid="focus-shot">
            <span className="tag plate" data-testid="focus-stream">
              {focus.streamTag}
            </span>
          </div>
          <figcaption>{focus.kind}</figcaption>
        </figure>
        <aside>
          <section className="card">
            <dl>
              {focus.rows.map((r) => (
                <div key={r.term} style={{ display: "contents" }}>
                  <dt>{r.term}</dt>
                  <dd>{r.value}</dd>
                </div>
              ))}
            </dl>
            <button type="button" className="btn primary" style={{ justifyContent: "center" }} disabled={focus.restart.disabled} onClick={onRestart} data-testid="restart-one">
              {focus.restart.label}
            </button>
          </section>
          <section className="card" aria-labelledby="readings-h">
            <h2 id="readings-h">{focus.readingsTitle}</h2>
            <div className="readings" data-testid="readings">
              {focus.readings.map((r) => (
                <div key={r.label} className="reading">
                  <span className="label">{r.label}</span>
                  <span className="v">{r.value}</span>
                  <svg viewBox="0 0 100 24" preserveAspectRatio="none" aria-hidden="true">
                    {r.points ? <polyline points={r.points} fill="none" stroke="var(--accent)" strokeWidth="1.5" vectorEffect="non-scaling-stroke" /> : null}
                  </svg>
                </div>
              ))}
            </div>
          </section>
          <section className="card" aria-labelledby="timeline-h">
            <div className="head">
              <h2 id="timeline-h">{focus.timelineTitle}</h2>
              <span className="total" data-testid="timeline-total">{focus.timelineTotal}</span>
            </div>
            <p className="note">{focus.timelineNote}</p>
            {focus.now ? <p className="now">{focus.now}</p> : null}
            <Timeline steps={focus.timeline} ec2={ec2} />
          </section>
        </aside>
      </div>
    </div>
  );
}
