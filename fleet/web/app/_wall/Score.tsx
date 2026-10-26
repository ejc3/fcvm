"use client";

// The race's two bars, fcvm against EC2, on one time axis, shown over the
// bottom of the image while a race runs. Every word and number comes from the
// view model.

import type { BarView, ScoreView } from "../../core/index.ts";

function pct(f: number): string {
  return `${(Math.max(0, Math.min(1, f)) * 100).toFixed(2)}%`;
}

function Bar({ b, compact }: { b: BarView; compact: boolean }) {
  const hatch = b.hatch > 0 ? <span className="hatch" style={{ width: pct(b.hatch) }} data-testid={`hatch-${b.half}`} title={b.hatchLabel} /> : null;
  const solid = <span className="solid" style={{ width: pct(b.solid) }} data-testid={`solid-${b.half}`} />;
  return (
    <div className={`bar-row ${b.half}${b.live ? " live" : ""}${b.done ? " done" : ""}`} data-testid={`bar-${b.half}`} data-solid={b.solid.toFixed(4)} data-hatch={b.hatch.toFixed(4)}>
      <span className="who">{b.name}</span>
      <span className="track" aria-hidden="true">
        {b.hatchFirst ? hatch : null}
        {solid}
        {b.hatchFirst ? null : hatch}
        {b.lastMark !== null ? <span className="last" style={{ left: pct(b.lastMark) }} /> : null}
      </span>
      <span className="figure">
        <span className="what">{b.valueLabel}</span>
        <span className="value" data-testid={`value-${b.half}`}>{b.value}</span>
      </span>
      {compact ? null : (
        <span className="line">
          <span data-testid={`line-${b.half}`}>{b.line}</span>
          {b.measured ? <span className="measured" data-testid={`measured-${b.half}`}>{b.measured}</span> : null}
          {b.hatchLabel ? <span className="hatch-label" data-testid={`hatch-label-${b.half}`}>{b.hatchLabel}</span> : null}
        </span>
      )}
    </div>
  );
}

export function Scoreboard({ score, compact }: { score: ScoreView; compact: boolean }) {
  return (
    <div className="bars" data-testid="scoreboard" role="group" aria-label={score.title}>
      {compact ? null : <span className="title">{score.title}</span>}
      <Bar b={score.fcvm} compact={compact} />
      <Bar b={score.ec2} compact={compact} />
      {compact ? (
        <span className="compact-lines">
          {[score.fcvm, score.ec2].map((b) => (
            <span key={b.half} className={b.half}>
              {b.name}: {b.line}
              {b.measured ? `, ${b.measured}` : ""}
              {b.hatchLabel ? `. ${b.hatchLabel}` : ""}
            </span>
          ))}
        </span>
      ) : null}
    </div>
  );
}
