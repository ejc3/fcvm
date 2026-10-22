"use client";

// Everything drawn over and around the frames: the line between the halves
// and its labels, the captions under each half, dark tiles' glyphs and phases,
// the stats switch's per-tile lines, the seams and the hovered tile's outline
// and label, banners and notices.

import type { CSSProperties } from "react";
import type { Glyph, TileView, WallView } from "../../core/index.ts";

function accent(color: string): CSSProperties {
  return { ["--accent" as string]: color };
}

/** A restart's progress as a ring of steps, or the mark for what is wrong. */
export function GlyphMark({ g, size }: { g: Glyph; size: number }) {
  const r = 9;
  if (g.kind === "failed") {
    return (
      <svg width={size} height={size} viewBox="0 0 24 24" aria-hidden="true" className="glyph failed">
        <circle cx="12" cy="12" r={r} fill="none" stroke="currentColor" strokeWidth="2" />
        <path d="M12 7v6M12 16.5v.5" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" />
      </svg>
    );
  }
  if (g.kind === "reclaimed") {
    return (
      <svg width={size} height={size} viewBox="0 0 24 24" aria-hidden="true" className="glyph reclaimed">
        <rect x="4" y="6" width="16" height="12" rx="2" fill="none" stroke="currentColor" strokeWidth="2" />
        <path d="M4 18L20 6" stroke="currentColor" strokeWidth="2" />
      </svg>
    );
  }
  if (g.kind === "borrowed") {
    return (
      <svg width={size} height={size} viewBox="0 0 24 24" aria-hidden="true" className="glyph borrowed">
        <circle cx="12" cy="8" r="3.5" fill="none" stroke="currentColor" strokeWidth="2" />
        <path d="M5 20c1-4 4-6 7-6s6 2 7 6" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
      </svg>
    );
  }
  if (g.kind === "capacity" || g.kind === "empty") {
    return (
      <svg width={size} height={size} viewBox="0 0 24 24" aria-hidden="true" className="glyph empty">
        <circle cx="12" cy="12" r={r} fill="none" stroke="currentColor" strokeWidth="2" strokeDasharray="3 3" />
      </svg>
    );
  }
  // Progress: one arc per step, the finished ones filled in.
  const gap = 0.18;
  const arcs = [];
  for (let i = 0; i < g.steps; i++) {
    const a0 = (i / g.steps) * Math.PI * 2 - Math.PI / 2 + gap / 2;
    const a1 = ((i + 1) / g.steps) * Math.PI * 2 - Math.PI / 2 - gap / 2;
    const p = (a: number) => `${(12 + r * Math.cos(a)).toFixed(2)} ${(12 + r * Math.sin(a)).toFixed(2)}`;
    arcs.push(
      <path
        key={i}
        d={`M ${p(a0)} A ${r} ${r} 0 0 1 ${p(a1)}`}
        fill="none"
        stroke="currentColor"
        strokeWidth={i < g.done ? 3 : 1.5}
        opacity={i < g.done ? 1 : 0.45}
      />,
    );
  }
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" aria-hidden="true" className="glyph progress" data-done={g.done} data-steps={g.steps}>
      {arcs}
    </svg>
  );
}

function DarkTile({ t }: { t: TileView }) {
  const b = t.box!;
  const big = b.h >= 64 && b.w >= 110;
  const size = Math.max(14, Math.min(34, b.h / 3.2));
  return (
    <div
      className={`dark-tile${t.hatch ? " hatched" : ""} ${t.glyph.kind}`}
      data-testid={`dark-${t.id}`}
      data-kind={t.glyph.kind}
      style={{ ...accent(t.accent), left: b.x, top: b.y, width: b.w, height: b.h }}
    >
      <GlyphMark g={t.glyph} size={size} />
      {big ? (
        <span className="phase plate" data-testid={`phase-${t.id}`}>
          {t.phase}
          {t.detail ? <span className="detail">{t.detail}</span> : null}
        </span>
      ) : null}
    </div>
  );
}

export function Overlays({ view, phone }: { view: WallView; phone: boolean }) {
  const L = view.layout;
  const showMosaic = L !== null && view.focus === null && view.own === null;
  const img = L?.image;
  const seams = view.seams;
  const statsOn = view.play?.stats.on ?? false;
  return (
    <div className="layer" data-testid="overlays">
      {showMosaic && img ? (
        <>
          <div className="half-line" style={{ left: L.halfLineX, top: img.y, height: img.h }} aria-hidden="true" />
          {view.tiles.map((t) => (t.dark && t.box ? <DarkTile key={t.id} t={t} /> : null))}
          {statsOn
            ? view.tiles.map((t) =>
                t.box ? (
                  <div key={`s-${t.id}`}>
                    <div className="tile-frame" style={{ left: t.box.x, top: t.box.y, width: t.box.w, height: t.box.h }} />
                    <div className="tile-stats" data-testid={`stats-${t.id}`} style={{ left: t.box.x, top: t.box.y, maxWidth: t.box.w - 8 }}>
                      {t.stats.map((line, i) => (
                        <div key={i} className={i === t.stats.length - 1 ? "kind" : undefined} style={i === t.stats.length - 1 ? { color: t.accent } : undefined}>
                          {line}
                        </div>
                      ))}
                    </div>
                  </div>
                ) : null,
              )
            : null}
          <svg className={`seams${seams.visible ? " on" : ""}`} data-testid="seams" data-visible={seams.visible ? "1" : "0"} aria-hidden="true">
            {seams.segments.map((s, i) => (
              <line key={i} x1={s.x0} y1={s.y0} x2={s.x1} y2={s.y1} stroke="rgba(255,255,255,0.25)" strokeWidth={seams.lineWidth} shapeRendering="crispEdges" />
            ))}
          </svg>
          {seams.outline ? (
            <div
              className="hover-outline"
              data-testid="hover-outline"
              data-tile={seams.label?.id}
              style={{ ...accent(seams.outline.color), left: seams.outline.box.x, top: seams.outline.box.y, width: seams.outline.box.w, height: seams.outline.box.h }}
            />
          ) : null}
          {view.labels.seam.map((l) => (
            <span
              key={l.half}
              data-avoid=""
              className={`seam-label plate ${l.half}`}
              data-testid={`seam-label-${l.half}`}
              style={l.align === "right" ? { right: `calc(100vw - ${l.x}px)`, top: l.y } : { left: l.x, top: l.y }}
            >
              {l.text}
            </span>
          ))}
          {phone
            ? null
            : view.labels.captions.map((c) => (
                <p
                  key={c.half}
                  className={`caption ${c.half}`}
                  data-testid={`caption-${c.half}`}
                  style={{ left: c.box.x, top: c.box.y, width: c.box.w, textAlign: c.half === "ec2" ? "right" : "left" }}
                >
                  <b>{c.tag}</b> {c.text}
                </p>
              ))}
          {seams.label ? (
            <div className="hover-label" data-testid="hover-label" style={{ ...accent(seams.label.accent), left: seams.label.x, top: seams.label.y }}>
              <span className="row1">
                <span className="id">{seams.label.id}</span>
                <span className="tag">{seams.label.kind}</span>
              </span>
              <span className="gpu">{seams.label.gpu}</span>
              <span className="host">{seams.label.host}</span>
              <span className="last">{seams.label.last}</span>
            </div>
          ) : null}
          {view.play?.light.on ? (
            <span className="light-hint plate" style={{ left: img.x + img.w / 2, top: img.y + img.h - 48 }}>
              {view.play.light.label}
            </span>
          ) : null}
          {statsOn ? (
            <span className="stats-note plate" data-testid="bandwidth" style={{ left: img.x + 12, top: img.y + img.h - 40 }}>
              {view.bandwidth}
            </span>
          ) : null}
        </>
      ) : null}
      {view.banners.length > 0 ? (
        <div className="banners" role="status">
          {view.banners.map((b) => (
            <div key={b.kind} className={`banner ${b.kind}`} data-testid={`banner-${b.kind}`}>
              {b.text}
            </div>
          ))}
        </div>
      ) : null}
      {view.notices.length > 0 ? (
        <div className="notices" role="status">
          {view.notices.map((n) => (
            <div key={n.key} className="notice">
              {n.text}
            </div>
          ))}
        </div>
      ) : null}
    </div>
  );
}
