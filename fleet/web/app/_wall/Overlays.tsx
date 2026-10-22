"use client";

// Everything drawn over the frames: the line between the halves, dark tiles'
// phase and clock, the stats switch's per-tile lines, the seams and the hovered
// tile's outline and label, the race, the action ticker, notices.

import type { CSSProperties } from "react";
import type { HalfView, WallView } from "../../core/index.ts";

function accent(color: string, soft?: string): CSSProperties {
  return { ["--accent" as string]: color, ...(soft ? { ["--soft" as string]: soft } : {}) };
}

function HalfPanel({ h, style }: { h: HalfView; style: CSSProperties }) {
  return (
    <div data-avoid="" className={`race-half ${h.half}`} style={{ ...accent(h.accent, h.soft), ...style }} data-testid={`race-${h.half}`}>
      <span className="by">{h.renderedBy}</span>
      <span className="back">{h.back}</span>
      <span className="progress" aria-hidden="true">
        <span style={{ width: `${Math.round(h.backFraction * 100)}%` }} />
      </span>
      <span className="stamp" data-testid={`stamp-${h.half}`}>{h.stamp}</span>
      <span className="median">{h.median}</span>
    </div>
  );
}

export function Overlays({ view, phone }: { view: WallView; phone: boolean }) {
  const L = view.layout;
  const showMosaic = L !== null && view.focus === null;
  const img = L?.image;
  const seams = view.seams;
  const statsOn = view.controls?.stats.on ?? false;
  return (
    <div className="layer" data-testid="overlays">
      {showMosaic && img ? (
        <>
          <div className="half-line" style={{ left: L.halfLineX, top: img.y, height: img.h }} aria-hidden="true" />
          {view.tiles.map((t) =>
            t.dark && t.box ? (
              <div
                key={t.id}
                className="dark-tile"
                data-testid={`dark-${t.id}`}
                style={{
                  left: t.box.x,
                  top: t.box.y,
                  width: t.box.w,
                  height: t.box.h,
                  color: t.accent,
                  ["--clock-size" as string]: `${Math.max(10, Math.min(22, t.box.h / 5)).toFixed(0)}px`,
                }}
              >
                {t.darkClock ? <span className="clock">{t.darkClock}</span> : null}
                {t.box.h >= 54 && t.box.w >= 90 ? <span className="phase">{t.darkPhase}</span> : null}
              </div>
            ) : null,
          )}
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
          {view.race && !phone ? (
            <>
              {statsOn ? null : <HalfPanel h={view.race.fcvm} style={{ left: img.x + 20, top: img.y + 20 }} />}
              {statsOn ? null : <HalfPanel h={view.race.ec2} style={{ right: `calc(100vw - ${img.x + img.w - 20}px)`, top: img.y + 20 }} />}
              <div data-avoid="" className={`race-clock${view.race.active ? " active" : ""}`} style={{ left: img.x + img.w / 2, top: img.y + 20 }} data-testid="race-clock">
                <div className="caption">{view.race.caption}</div>
                <div className="value">{view.race.clock}</div>
                {view.race.by ? <div className="started">{view.race.by}</div> : null}
              </div>
            </>
          ) : null}
          {seams.label ? (
            <div
              className="hover-label"
              data-testid="hover-label"
              style={{ ...accent(seams.label.accent), left: seams.label.x, top: seams.label.y }}
            >
              <span className="top">
                <span className="id">{seams.label.id}</span>
                <span className="tag">{seams.label.kind}</span>
              </span>
              <span className="gpu">{seams.label.gpu}</span>
              <span className="host">{seams.label.host}</span>
              <span className="last">{seams.label.last}</span>
            </div>
          ) : null}
          {!phone && view.controls ? (
            <span data-avoid="" className="hint" style={{ right: `calc(100vw - ${img.x + img.w - 16}px)`, bottom: `calc(100dvh - ${img.y + img.h - 16}px)` }}>
              {view.controls.word.status ? (
                <span className={`word-status ${view.controls.word.statusKind}`} role="status" data-testid="word-status">
                  {view.controls.word.status}
                </span>
              ) : null}
              <span className={view.controls.latencyMeasured ? "latency" : undefined} data-testid="latency">{view.controls.latency}</span>
              {statsOn ? <span data-testid="bandwidth">{view.bandwidth}</span> : null}
              {statsOn ? <span data-testid="mode-detail">{`${view.controls.mode.detail}. ${view.controls.mode.suggestion.text}`}</span> : null}
              <span>{view.controls.help}</span>
            </span>
          ) : null}
          {!phone && view.ticker.length > 0 ? (
            <div data-avoid="" className="ticker" style={{ left: img.x + 20, bottom: `calc(100dvh - ${img.y + img.h - 16}px)` }} aria-label="Recent actions">
              {view.ticker.map((a) => (
                <span key={a.key}>
                  {a.text}
                  <span className="when">{a.when}</span>
                </span>
              ))}
            </div>
          ) : null}
        </>
      ) : null}
      {view.banner ? <div className="banner" role="status">{view.banner}</div> : null}
    </div>
  );
}
