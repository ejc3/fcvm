// A stream's wire cost and pace as the take-over panel shows them: bits per second over the
// last second, the bytes since the meter started, and events per second (frames shown, or
// calls the phone's GPU answered). Time is passed in; nothing here reads a clock.

export const METER_WINDOW_MS = 1000;

export class RateMeter {
  private readonly times: number[] = [];
  private readonly sizes: number[] = [];
  private head = 0;
  private inWindow = 0;
  /** Every byte (or event count) added since the meter started or was reset. */
  total = 0;
  /** When the first sample arrived, for a rate over a window shorter than one second. */
  private startedAt: number | null = null;

  /** Adds `n` bytes (or events) at `now` (ms). */
  add(now: number, n: number): void {
    this.startedAt ??= now;
    this.times.push(now);
    this.sizes.push(n);
    this.inWindow += n;
    this.total += n;
    this.trim(now);
  }

  /** The sum over the last second, per second. A meter younger than a second divides by its age. */
  perSecond(now: number): number {
    this.trim(now);
    if (this.startedAt === null) return 0;
    const span = Math.min(METER_WINDOW_MS, Math.max(now - this.startedAt, 250));
    return (this.inWindow * 1000) / span;
  }

  reset(): void {
    this.times.length = 0;
    this.sizes.length = 0;
    this.head = 0;
    this.inWindow = 0;
    this.total = 0;
    this.startedAt = null;
  }

  private trim(now: number): void {
    while (this.head < this.times.length && this.times[this.head]! <= now - METER_WINDOW_MS) {
      this.inWindow -= this.sizes[this.head]!;
      this.head++;
    }
    if (this.head > 1024 && this.head * 2 > this.times.length) {
      this.times.splice(0, this.head);
      this.sizes.splice(0, this.head);
      this.head = 0;
    }
  }
}

/** Mbit/s from bytes per second, as the panel prints it. */
export function mbits(bytesPerSecond: number): string {
  const m = (bytesPerSecond * 8) / 1e6;
  return m >= 100 ? m.toFixed(0) : m >= 10 ? m.toFixed(1) : m.toFixed(2);
}

/** A byte count since take-over, as the panel prints it. */
export function megabytes(bytes: number): string {
  const m = bytes / 1e6;
  return m >= 1000 ? `${(m / 1000).toFixed(2)} GB` : m >= 100 ? `${m.toFixed(0)} MB` : `${m.toFixed(1)} MB`;
}
