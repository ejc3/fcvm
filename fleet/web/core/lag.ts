// Where a tile's frame age goes on the page. For each frame the drawer records when its
// message arrived, when the decoder gave it back, and when a draw first showed it in step
// with the other tiles. Each time is kept as local Unix ms minus the frame's scene time in
// ms, so a percentile of it is an age on the local clock; the page adds the coordinator
// clock offset and the scene epoch (sceneAgeMs) to make it the age the viewer sees.

/** Frames whose times a tile keeps: 3 s at 30 fps. */
export const LAG_FRAMES_KEPT = 90;

interface Times {
  t: number;
  arrive: number;
  decoded: number | null;
  drawn: number | null;
}

/** Per tile, the p50 of each stage's local time minus scene time, ms. */
export interface LagStats {
  arrive: number;
  decoded: number | null;
  drawn: number | null;
  /** Frames the percentiles are over (arrive's; the later stages have at most this many). */
  n: number;
}

const key = (t: number) => Math.round(t * 1e6);

function p50(xs: number[]): number | null {
  if (xs.length === 0) return null;
  const s = [...xs].sort((a, b) => a - b);
  return s[Math.floor(s.length / 2)]!;
}

export class FrameLog {
  private readonly byT = new Map<number, Times>();

  /** A frame message for scene time `t` arrived at local Unix ms `at`. */
  arrived(t: number, at: number): void {
    const k = key(t);
    if (this.byT.has(k)) return;
    this.byT.set(k, { t, arrive: at, decoded: null, drawn: null });
    while (this.byT.size > LAG_FRAMES_KEPT) this.byT.delete(this.byT.keys().next().value!);
  }

  decoded(t: number, at: number): void {
    const e = this.byT.get(key(t));
    if (e && e.decoded === null) e.decoded = at;
  }

  /** A draw showed this frame in step with the others. Only the first draw counts. */
  drawn(t: number, at: number): void {
    const e = this.byT.get(key(t));
    if (e && e.drawn === null) e.drawn = at;
  }

  clear(): void {
    this.byT.clear();
  }

  summary(): LagStats | null {
    const arrive: number[] = [];
    const decoded: number[] = [];
    const drawn: number[] = [];
    for (const e of this.byT.values()) {
      const ms = e.t * 1000;
      arrive.push(e.arrive - ms);
      if (e.decoded !== null) decoded.push(e.decoded - ms);
      if (e.drawn !== null) drawn.push(e.drawn - ms);
    }
    const a = p50(arrive);
    if (a === null) return null;
    return { arrive: a, decoded: p50(decoded), drawn: p50(drawn), n: arrive.length };
  }
}

/** A recorded local-minus-scene time as the frame's age on the coordinator's clock, ms. */
export function sceneAgeMs(localMinusSceneMs: number, offsetMs: number, epochUnixMs: number): number {
  return localMinusSceneMs + offsetMs - epochUnixMs;
}
