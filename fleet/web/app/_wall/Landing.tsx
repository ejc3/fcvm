"use client";

// The line under a page's title and its "What is this?" control. One component for all five
// pages; the words come from core/landing.ts.

import { createPortal } from "react-dom";
import { useEffect, useId, useRef, useState, type CSSProperties, type ReactNode } from "react";
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
export function Landing({ landing, legendInPanel, colour, sheet = "bottom", children }: { landing: LandingView; legendInPanel?: boolean; colour: string; sheet?: "top" | "bottom"; children?: ReactNode }) {
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
    window.addEventListener("keydown", key, true);
    return () => window.removeEventListener("keydown", key, true);
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
        <>
        {/* A tap outside takes the whole pointer sequence (down, up, click) and only closes the panel: nothing under it sees it. */}
        <div className="landing-backdrop" data-testid="landing-backdrop" onPointerDown={(e) => e.stopPropagation()} onPointerUp={(e) => e.stopPropagation()} onClick={(e) => { e.stopPropagation(); setOpen(false); }} />
        <div ref={panelRef} id={panelId} className="landing-panel" data-sheet={sheet} role="dialog" aria-label="What this page shows" tabIndex={-1} data-testid="landing-panel" style={{ ["--lx" as string]: `${Math.min(anchor?.x ?? 0, Math.max(0, window.innerWidth - 436))}px`, ["--ly" as string]: `${anchor?.y ?? 0}px` }}>
          <button type="button" className="landing-close" aria-label="Close" data-testid="landing-close" onClick={() => { setOpen(false); btnRef.current?.focus(); }}>
            Close
          </button>
          {text.map((t) => (
            <p key={t}>{t}</p>
          ))}
          {legendInPanel && landing.legend ? <PotatoLegend legend={landing.legend} form="list" colour={colour} /> : null}
        </div>
        </>,
        (btnRef.current?.closest("[data-landing-root]") as HTMLElement | null) ?? document.body,
      )
        : null}
      {children}
    </div>
  );
}
