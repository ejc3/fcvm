"use client";

// The line under a page's title and its "What is this?" control. One component for all five
// pages; the words come from core/landing.ts.

import { useEffect, useId, useLayoutEffect, useRef, useState, type CSSProperties, type ReactNode } from "react";
import { landingPanelText, type LandingView, type LegendEntry } from "../../core/index.ts";
import "./landing.css";

/** A legend mark, drawn with the Potato tiles' own phase styles. */
function Mark({ m, colour }: { m: LegendEntry["mark"]; colour: string }) {
  if (m.kind === "hold-bar") {
    return (
      <span className="lg-meter" aria-hidden="true">
        <span className="fill" style={{ width: "62%" }} />
      </span>
    );
  }
  if (m.kind === "hold-tick") {
    return (
      <span className="lg-meter" aria-hidden="true">
        <span className="fill" style={{ width: "62%" }} />
        <span className="need" style={{ left: "80%" }} />
      </span>
    );
  }
  return (
    <span className="potato-swatch" data-phase={m.phase} data-holder={m.holder ? "true" : undefined} style={{ ["--c" as string]: colour } as CSSProperties} aria-hidden="true">
      {m.wake !== undefined ? (
        <span className="wake-bar">
          <span style={{ width: `${m.wake * 100}%` }} />
        </span>
      ) : null}
      {m.holder ? <span className="lg-ball" /> : null}
    </span>
  );
}

function Label({ e }: { e: LegendEntry }) {
  return (
    <>
      {e.label !== null ? <span className="lg-label">{e.label}</span> : <span className="sr-only">{e.sr}</span>}
      {e.detail ? <span className="lg-detail">{e.detail}</span> : null}
    </>
  );
}

/** The legend: a strip cut from the ring and a small diagram of the Hold meter, or the same entries as a list. */
export function PotatoLegend({ legend, form, colour, id }: { legend: NonNullable<LandingView["legend"]>; form: "strip" | "list"; colour: string; id?: string }) {
  const h = useId();
  if (form === "list") {
    return (
      <section className="lg lg-list" aria-labelledby={h} data-testid="potato-legend" id={id}>
        <h2 id={h}>{legend.title}</h2>
        <ul>
          {[...legend.strip, ...legend.hold].map((e) => (
            <li key={e.key}>
              <Mark m={e.mark} colour={colour} />
              <span className="lg-text">
                <Label e={e} />
              </span>
            </li>
          ))}
          <li>
            <span className="lg-text">{legend.catchNote}</span>
          </li>
        </ul>
      </section>
    );
  }
  return (
    <section className="card lg lg-strip" aria-labelledby={h} data-testid="potato-legend" id={id}>
      <h2 id={h}>{legend.title}</h2>
      <ol className="lg-row lg-ring">
        {legend.strip.map((e) => (
          <li key={e.key}>
            <Mark m={e.mark} colour={colour} />
            {e.label !== null ? <span className="lg-label">{e.label}</span> : <span className="sr-only">{e.sr}</span>}
          </li>
        ))}
      </ol>
      {legend.strip
        .filter((e) => e.detail)
        .map((e) => (
          <p key={e.key} className="lg-note">
            {e.label}: {e.detail}
          </p>
        ))}
      <p className="lg-note">{legend.catchNote}</p>
      <ol className="lg-row lg-hold">
        {legend.hold.map((e) => (
          <li key={e.key}>
            <Mark m={e.mark} colour={colour} />
            <Label e={e} />
          </li>
        ))}
      </ol>
    </section>
  );
}

/** The page's line, and the "What is this?" button with its panel (a popover on a wide screen, a bottom sheet on a phone). */
export function Landing({ landing, legendInPanel, colour, children }: { landing: LandingView; legendInPanel?: boolean; colour: string; children?: ReactNode }) {
  const [open, setOpen] = useState(false);
  const [clamped, setClamped] = useState(false);
  const lineRef = useRef<HTMLParagraphElement | null>(null);
  const btnRef = useRef<HTMLButtonElement | null>(null);
  const panelRef = useRef<HTMLDivElement | null>(null);
  const panelId = useId();

  // The line is clamped to two lines; when it is cut, the panel carries it whole.
  useLayoutEffect(() => {
    const el = lineRef.current;
    if (!el) return;
    const measure = () => setClamped(el.scrollHeight > el.clientHeight + 1);
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, [landing.line]);

  useEffect(() => {
    if (!open) return;
    panelRef.current?.focus();
    const close = (back: boolean) => {
      setOpen(false);
      if (back) btnRef.current?.focus();
    };
    const key = (e: KeyboardEvent) => {
      if (e.key === "Escape") close(true);
    };
    const down = (e: PointerEvent) => {
      const t = e.target as Node;
      if (!panelRef.current?.contains(t) && !btnRef.current?.contains(t)) close(false);
    };
    document.addEventListener("keydown", key);
    document.addEventListener("pointerdown", down);
    return () => {
      document.removeEventListener("keydown", key);
      document.removeEventListener("pointerdown", down);
    };
  }, [open]);

  const text = landingPanelText(landing, clamped);
  return (
    <div className="landing" data-testid="landing">
      <p ref={lineRef} className="landing-line" data-testid="landing-line">
        {landing.line}
      </p>
      <button ref={btnRef} type="button" className="landing-btn" aria-expanded={open} aria-controls={open ? panelId : undefined} data-testid="landing-button" onClick={() => setOpen((o) => !o)}>
        {landing.button}
      </button>
      {open ? (
        <div ref={panelRef} id={panelId} className="landing-panel" role="dialog" aria-label={landing.button} tabIndex={-1} data-testid="landing-panel">
          {text.map((t) => (
            <p key={t}>{t}</p>
          ))}
          {legendInPanel && landing.legend ? <PotatoLegend legend={landing.legend} form="list" colour={colour} /> : null}
        </div>
      ) : null}
      {children}
    </div>
  );
}
