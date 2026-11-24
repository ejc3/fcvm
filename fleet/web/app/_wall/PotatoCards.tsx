"use client";

// The Potato page's figures: the CPU slots, machines per CPU, the pace chart and the ladder
// bars. Every word comes from core/potato.ts; a figure the system does not send is not drawn.

import type { CSSProperties } from "react";
import type { ChartView, CpuSlotsView, DensityView, LadderBarsView, SlotGroupKey } from "../../core/index.ts";

const SLOT_PHASE: Record<Exclude<SlotGroupKey, "free">, "awake" | "waking" | "sleeping"> = { ball: "awake", ready: "awake", waking: "waking", saving: "sleeping" };

/**
 * The K CPUs as slots, each holding the chip of the machine that holds it. "full": grouped and
 * labelled; "compact": small chips in one or two rows, the group labels spoken and shown on hover.
 */
export function CpuSlots({ slots, form = "full", rows = 1, chip = 36 }: { slots: CpuSlotsView; form?: "full" | "compact"; rows?: number; chip?: number }) {
  const compact = form === "compact";
  const cols = Math.ceil(slots.k / rows);
  return (
    <div className={`potato-cpus${compact ? " compact" : ""}`} data-testid="potato-cpus" data-form={form} style={{ ["--chip" as string]: `${chip}px` } as CSSProperties}>
      <p className="cpus-heading" data-testid="potato-cpus-heading">
        {slots.heading}
        {slots.busy ? (
          <span className="busy" data-testid="potato-cpus-busy">
            {slots.busy}
          </span>
        ) : null}
      </p>
      <ol className="slot-groups" style={compact ? { width: cols * 23 } : undefined}>
        {slots.groups.map((g) => (
          <li key={g.key} className="slot-group" data-group={g.key}>
            <span className="slot-row">
              {g.slots.map((s, i) =>
                s.player === null ? (
                  <span key={i} className="slot free" data-testid="potato-slot" data-group="free" data-label={compact ? g.label : undefined} />
                ) : (
                  <span
                    key={s.player}
                    className="potato-swatch sm"
                    data-testid="potato-slot"
                    data-group={g.key}
                    data-phase={SLOT_PHASE[g.key as Exclude<SlotGroupKey, "free">]}
                    data-holder={g.key === "ball" ? "true" : undefined}
                    data-label={compact ? `${g.label}: player ${s.number}` : undefined}
                    style={{ ["--c" as string]: s.colour } as CSSProperties}
                  >
                    <span className="num">{s.number}</span>
                  </span>
                ),
              )}
            </span>
            <span className={compact ? "sr-only" : "slot-label"}>{g.label}</span>
          </li>
        ))}
      </ol>
      {slots.asleep && !compact ? <p className="cpus-asleep">{slots.asleep}</p> : null}
    </div>
  );
}

/** Machines per CPU in the large type, then the figures the system measures. */
export function DensityCard({ d }: { d: DensityView }) {
  return (
    <section className="card potato-density" aria-labelledby="potato-density-h" data-testid="potato-density">
      <div className="head">
        <h2 id="potato-density-h">{d.heading}</h2>
        <span className="where" data-testid="potato-where">
          {d.where}
        </span>
      </div>
      <div className="big-row">
        <span className="big" data-testid="potato-big">
          {d.big}
        </span>
        {d.machines ? (
          <span className="machines" data-testid="potato-machines">
            {d.machines}
          </span>
        ) : null}
        {d.record ? (
          <span className="record-pill" data-testid="potato-record">
            record <span className="was">was {d.record.was}</span>
          </span>
        ) : null}
      </div>
      {d.figures.length > 0 ? (
        <dl className="figures">
          {d.figures.map((f) => (
            <div key={f.key} data-testid={`potato-figure-${f.key}`}>
              <dd className="v">{f.value}</dd>
              <dt>{f.label}</dt>
            </div>
          ))}
        </dl>
      ) : null}
    </section>
  );
}

const W = 320;
const H = 150;
const PAD = { l: 30, r: 8, t: 20, b: 28 };

/** CPUs in use against pace, with the allotment as the line no wake can pass. */
export function ChartCard({ c }: { c: ChartView }) {
  const x = (pace: number) => PAD.l + (pace / c.paceMax) * (W - PAD.l - PAD.r);
  const y = (cpus: number) => H - PAD.b - (cpus / c.cpusMax) * (H - PAD.t - PAD.b);
  const pts = [...c.points].sort((a, b) => a.pace - b.pace);
  let d = "";
  pts.forEach((p, i) => {
    d += i === 0 ? `M${x(p.pace)} ${y(p.cpus)}` : `H${x(p.pace)}V${y(p.cpus)}`;
  });
  return (
    <div className="potato-chart" data-testid="potato-chart">
      <h3>{c.title}</h3>
      <svg viewBox={`0 0 ${W} ${H}`} role="img" aria-label={`${c.title}: ${c.points.length} laps, ${c.k} ${c.noun} is the limit`}>
        <line className="axis" x1={PAD.l} y1={H - PAD.b} x2={W - PAD.r} y2={H - PAD.b} />
        <line className="k-line" x1={PAD.l} y1={y(c.k)} x2={W - PAD.r} y2={y(c.k)} />
        <text className="k-label" x={PAD.l + 4} y={y(c.k) - 5}>
          {c.kLabel}
        </text>
        <path className="steps" d={d} />
        {pts.length > 0 ? <line className="drop-line" x1={x(pts[0]!.pace)} y1={y(pts[0]!.cpus)} x2={x(pts[0]!.pace)} y2={y(0)} /> : null}
        {pts.map((p) => (
          <circle key={`${p.pace}-${p.cpus}`} className="dot" cx={x(p.pace)} cy={y(p.cpus)} r="3" />
        ))}
        {c.now ? <circle className="now" cx={x(c.now.pace)} cy={y(c.now.cpus)} r="5" /> : null}
        <text className="tick" x={PAD.l - 4} y={y(0) + 5} textAnchor="end">
          0
        </text>
        <text className="tick" x={PAD.l - 4} y={y(c.k) + 5} textAnchor="end">
          {c.k}
        </text>
        <text className="tick" x={x(0)} y={H - 6}>
          0
        </text>
        <text className="tick" x={W - PAD.r} y={H - 6} textAnchor="end">
          {c.paceMax} catches / s
        </text>
      </svg>
    </div>
  );
}

/** One bar per rung: its length is the fastest pace held at that ring size. */
export function LadderBars({ l }: { l: LadderBarsView }) {
  return (
    <div className="potato-ladder-bars" data-testid="potato-ladder">
      <h3>{l.title}</h3>
      <ol>
        {l.rows.map((r) => (
          <li key={r.size} aria-current={r.current ? "true" : undefined} data-skipped={r.skipped ? "true" : undefined} data-testid={`potato-rung-${r.size}`}>
            <span className="size">{r.size}</span>
            <span className="per">{r.label}</span>
            <span className="bar" aria-hidden="true">
              {r.length !== null ? <span className="fill" style={{ width: `${(r.length * 100).toFixed(1)}%` }} /> : null}
            </span>
            <span className="val" data-testid="potato-rung-value">
              {r.value}
            </span>
          </li>
        ))}
      </ol>
      <p className="unit">{l.unit}</p>
    </div>
  );
}
