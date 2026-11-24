"use client";

// The line under a page's title and its "What is this?" control. One component for all five
// pages; the words come from core/landing.ts.

import { createPortal } from "react-dom";
import { useEffect, useId, useRef, useState, type CSSProperties, type ReactNode } from "react";
import { landingPanelText, type LandingView, type LegendEntry } from "../../core/index.ts";
import "./landing.css";

/** A legend mark, drawn with the Potato tiles' own phase styles. */
function Mark({ m, colour }: { m: LegendEntry["mark"]; colour: string }) {
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

/** The legend as a list, for the "What is this?" panel: the phases' tints and what a colour step means. */
export function PotatoLegend({ legend, colour, id }: { legend: NonNullable<LandingView["legend"]>; colour: string; id?: string }) {
  const h = useId();
  return (
    <section className="lg lg-list" aria-labelledby={h} data-testid="potato-legend" id={id}>
      <h2 id={h}>{legend.title}</h2>
      <ul>
        {legend.strip.map((e) => (
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

/** The page's line, and the "What is this?" button with its panel (a popover on a wide screen, a bottom sheet on a phone). */
export function Landing({ landing, colour, sheet = "bottom", children }: { landing: LandingView; colour: string; sheet?: "top" | "bottom"; children?: ReactNode }) {
  const [open, setOpen] = useState(false);
  const btnRef = useRef<HTMLButtonElement | null>(null);
  const panelRef = useRef<HTMLDivElement | null>(null);
  const panelId = useId();
  const [anchor, setAnchor] = useState<{ x: number; y: number } | null>(null);

  useEffect(() => {
    if (!open) return;
    panelRef.current?.focus();
    const close = (back: boolean) => {
      setOpen(false);
      if (back) btnRef.current?.focus();
    };
    // The topmost layer takes Escape: the panel closes and nothing under it (a taken-over server, an opened phone) sees the key.
    const key = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.stopPropagation();
      e.preventDefault();
      close(true);
    };
    // A tap outside only closes: the click that follows it is swallowed.
    const down = (e: PointerEvent) => {
      const t = e.target as Node;
      if (panelRef.current?.contains(t) || btnRef.current?.contains(t)) return;
      const swallow = (c: Event) => c.stopPropagation();
      window.addEventListener("click", swallow, { capture: true, once: true });
      window.setTimeout(() => window.removeEventListener("click", swallow, true), 400);
      close(false);
    };
    window.addEventListener("keydown", key, true);
    window.addEventListener("pointerdown", down, true);
    return () => {
      window.removeEventListener("keydown", key, true);
      window.removeEventListener("pointerdown", down, true);
    };
  }, [open]);

  const text = landingPanelText(landing);
  return (
    <div className="landing" data-testid="landing">
      <p className="landing-line" data-testid="landing-line">
        <span data-testid="landing-text">{landing.line}</span>{" "}
        <button ref={btnRef} type="button" className="landing-btn" aria-expanded={open} aria-controls={open ? panelId : undefined} data-testid="landing-button" onClick={() => {
          const r = btnRef.current?.getBoundingClientRect();
          if (r) setAnchor({ x: r.left, y: r.bottom + 4 });
          setOpen((o) => !o);
        }}>
          {landing.button}
        </button>
      </p>
      {open && typeof document !== "undefined"
        ? createPortal(
        <div ref={panelRef} id={panelId} className="landing-panel" data-sheet={sheet} role="dialog" aria-label="About this page" tabIndex={-1} data-testid="landing-panel" style={{ ["--lx" as string]: `${Math.min(anchor?.x ?? 0, Math.max(0, window.innerWidth - 436))}px`, ["--ly" as string]: `${anchor?.y ?? 0}px` }}>
          <button type="button" className="landing-close" aria-label="Close" data-testid="landing-close" onClick={() => { setOpen(false); btnRef.current?.focus(); }}>
            Close
          </button>
          {text.map((t) => (
            <p key={t}>{t}</p>
          ))}
          {landing.legend ? <PotatoLegend legend={landing.legend} colour={colour} /> : null}
        </div>,
        document.body,
      )
        : null}
      {children}
    </div>
  );
}
