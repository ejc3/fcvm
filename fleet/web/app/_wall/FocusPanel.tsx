"use client";

// One VM opened: the canvas zooms the image until that VM fills the screen, with the
// tiles around it dimmed (core/view.ts, plan.zoom). This overlay draws what sits over
// the picture: a bar with the way back, an outline on the VM, and a sheet with its
// details, restart timeline and live GPU readings. The sheet lies over the picture
// (a side sheet on a wide screen, a bottom sheet on a phone) and folds away.

import { useEffect, useRef, type CSSProperties } from "react";
import type { Direction, FocusView, TimelineStep } from "../../core/index.ts";

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

const ARROWS: Record<string, Direction> = { ArrowLeft: "left", ArrowRight: "right", ArrowUp: "up", ArrowDown: "down" };

export function FocusPanel({
  focus,
  phone,
  sheetOpen,
  onSheet,
  onClose,
  onRestart,
  onPick,
  onMove,
}: {
  focus: FocusView;
  phone: boolean;
  /** The sheet is open (a wide screen) or expanded (a phone). */
  sheetOpen: boolean;
  onSheet: (open: boolean) => void;
  onClose: () => void;
  onRestart: () => void;
  /** A click on the picture: the page opens the tile under it, if it is another. */
  onPick: (x: number, y: number) => void;
  /** An arrow key: the page opens the neighbouring tile that way. */
  onMove: (dir: Direction) => void;
}) {
  const backRef = useRef<HTMLButtonElement | null>(null);
  useEffect(() => {
    backRef.current?.focus();
  }, []);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        onClose();
        return;
      }
      const dir = ARROWS[e.key];
      const el = e.target as HTMLElement | null;
      // Arrows still scroll the sheet and move through its controls.
      if (!dir || el?.closest(".sheet") || el?.closest("input, textarea, select")) return;
      e.preventDefault();
      onMove(dir);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose, onMove]);
  // A tap that ends a drag on the picture is not a pick.
  const down = useRef<{ x: number; y: number } | null>(null);
  const accent = { ["--accent" as string]: focus.accent } as CSSProperties;
  const ec2 = focus.id.startsWith("ec2");
  const box = focus.placed?.box ?? null;
  const summary = focus.rows.find((r) => r.term === "Up for");
  return (
    <div className={`zoom${phone ? " phone-zoom" : ""}`} role="dialog" aria-modal="true" aria-labelledby="focus-title" data-testid="focus" style={accent}>
      <div
        className="zoom-hit"
        data-testid="zoom-hit"
        onPointerDown={(e) => {
          down.current = { x: e.clientX, y: e.clientY };
        }}
        onPointerUp={(e) => {
          const d = down.current;
          down.current = null;
          if (d && Math.hypot(e.clientX - d.x, e.clientY - d.y) <= 8) onPick(e.clientX, e.clientY);
        }}
      />
      {box ? (
        <div className="zoom-ring" data-testid="focus-shot" style={{ left: box.x, top: box.y, width: box.w, height: box.h }}>
          <span className="tag plate" data-testid="focus-stream">
            {focus.streamTag}
          </span>
        </div>
      ) : null}
      <nav className="zoom-bar">
        <button ref={backRef} type="button" className="btn" onClick={onClose}>
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
            <path d="M15 18l-6-6 6-6" />
          </svg>
          {focus.back}
        </button>
        <h1 id="focus-title">{focus.title}</h1>
        <span className="state" data-testid="focus-state">{focus.state}</span>
        {!phone && !sheetOpen ? (
          <button type="button" className="btn sheet-open" aria-expanded="false" aria-controls="focus-sheet" onClick={() => onSheet(true)} data-testid="sheet-toggle">
            Details
          </button>
        ) : null}
      </nav>
      <aside id="focus-sheet" className={`sheet${sheetOpen ? " open" : ""}`} data-testid="focus-sheet" aria-label={`${focus.title} details`}>
        <div className="sheet-head">
          {phone ? (
            <button type="button" className="sheet-handle" aria-expanded={sheetOpen} aria-controls="focus-sheet" onClick={() => onSheet(!sheetOpen)} data-testid="sheet-toggle">
              <span className="grip" aria-hidden="true" />
              <span className="peek">
                <span className="kind">{focus.kind}</span>
                {summary ? (
                  <span className="up">
                    {summary.term} {summary.value}
                  </span>
                ) : null}
              </span>
              <span className="more">{sheetOpen ? "Less" : "Details"}</span>
            </button>
          ) : (
            <>
              <p className="kind">{focus.kind}</p>
              <button type="button" className="btn icon" aria-label="Hide details" aria-expanded="true" aria-controls="focus-sheet" onClick={() => onSheet(false)} data-testid="sheet-hide">
                <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
                  <path d="M9 18l6-6-6-6" />
                </svg>
              </button>
            </>
          )}
        </div>
        <div className="sheet-body" inert={!sheetOpen}>
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
        </div>
      </aside>
    </div>
  );
}
