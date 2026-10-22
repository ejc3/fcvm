"use client";

// One tile opened large. The canvas draws the tile into the measured shot box;
// this panel draws the frame around it and the tile's details.

import { useEffect, useRef, type CSSProperties, type RefObject } from "react";
import type { FocusView, WallView } from "../../core/index.ts";

const BLUES = ["#BFE3FF", "#7CC4FF", "#3E95D6", "#2A6FA3", "#1B4A6E", "#123552"];
const ORANGES = ["#FFD089", "#F5A524", "#B8780F", "#7A4F08", "#5A3A06", "#3F2904"];

export function FocusPanel({
  view,
  focus,
  shotRef,
  onClose,
  onRelaunch,
}: {
  view: WallView;
  focus: FocusView;
  shotRef: RefObject<HTMLDivElement | null>;
  onClose: () => void;
  onRelaunch: () => void;
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
  const shades = (title: string) => (title.includes("(EC2)") ? ORANGES : BLUES);
  return (
    <div className="focus" role="dialog" aria-modal="true" aria-labelledby="focus-title" data-testid="focus" style={accent}>
      <nav>
        <button ref={backRef} type="button" className="btn" onClick={onClose}>
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
            <path d="M15 18l-6-6 6-6" />
          </svg>
          {focus.back}
        </button>
        <span className="where">{focus.nav}</span>
      </nav>
      <div className="body">
        <figure>
          <div className="shot" ref={shotRef} data-testid="focus-shot">
            <div className="tags">
              <span data-testid="focus-stream">{focus.streamTag}</span>
              <span>{focus.sourceTag}</span>
            </div>
          </div>
          <figcaption>{focus.caption}</figcaption>
        </figure>
        <aside>
          <section className="card">
            <div className="head">
              <span className="dot" aria-hidden="true" />
              <h1 id="focus-title">{focus.title}</h1>
              <span style={{ color: "var(--muted)", fontSize: 14 }}>{focus.state}</span>
            </div>
            <dl>
              {focus.rows.map((r) => (
                <div key={r.term} style={{ display: "contents" }}>
                  <dt>{r.term}</dt>
                  <dd>{r.value}</dd>
                </div>
              ))}
            </dl>
            <button type="button" className="btn primary" style={{ justifyContent: "center" }} disabled={focus.relaunch.disabled} onClick={onRelaunch}>
              {focus.relaunch.label}
            </button>
          </section>
          <section className="card" aria-labelledby="last-h">
            <div style={{ display: "flex", alignItems: "baseline", justifyContent: "space-between", gap: 12 }}>
              <h2 id="last-h">{focus.lastTitle}</h2>
              <span className="total">{focus.lastValue}</span>
            </div>
            <p className="note">{focus.lastNote}</p>
            {focus.bars.map((b) => (
              <div key={b.title} style={{ display: "flex", flexDirection: "column", gap: 6 }}>
                <span style={{ fontSize: 13, fontWeight: 600, color: b.accent }}>
                  {b.title} · {b.total}
                </span>
                <span className="phase-bar">
                  {b.segments.map((s, i) => (
                    <span key={i} title={s.label} style={{ width: `${(s.fraction * 100).toFixed(2)}%`, background: shades(b.title)[s.shade % 6] }} />
                  ))}
                </span>
              </div>
            ))}
            <ol className="legend">
              {focus.bars.flatMap((b) =>
                b.segments.map((s, i) => (
                  <li key={`${b.title}-${i}`}>
                    <span style={{ color: "var(--muted)" }}>
                      <span className="sw" style={{ background: shades(b.title)[s.shade % 6] }} aria-hidden="true" />
                      {s.label}
                    </span>
                    <span className="v">{s.value}</span>
                  </li>
                )),
              )}
            </ol>
          </section>
        </aside>
      </div>
      <span className="sr-only">{view.title}</span>
    </div>
  );
}
