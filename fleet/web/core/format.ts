// Number and time formatting. Every number the page shows goes through here.

/** A relaunch duration: "2.8 s" below 10 s, "58 s" below 10 minutes, then minutes. */
export function seconds(ms: number): string {
  const s = Math.max(0, ms) / 1000;
  if (s < 10) return `${s.toFixed(1)} s`;
  if (s < 600) return `${Math.round(s)} s`;
  return `${Math.round(s / 60)} min`;
}

/** A running race clock: "0:04.2", "1:07.9", "12:03.0". Tenths are truncated, never rounded up. */
export function raceClock(ms: number): string {
  const tenths = Math.floor(Math.max(0, ms) / 100);
  const m = Math.floor(tenths / 600);
  const s = Math.floor((tenths % 600) / 10);
  const d = tenths % 10;
  return `${m}:${String(s).padStart(2, "0")}.${d}`;
}

/** A countdown: "1:59", "0:05". */
export function countdown(ms: number): string {
  const total = Math.max(0, Math.ceil(ms / 1000));
  return `${Math.floor(total / 60)}:${String(total % 60).padStart(2, "0")}`;
}

/** Milliseconds as "182 ms", or "1.4 s" from a second up. */
export function millis(ms: number): string {
  if (ms < 1000) return `${Math.round(ms)} ms`;
  return `${(ms / 1000).toFixed(1)} s`;
}

/** A span: "4 s", "3 min", "2 h". */
export function span(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 60) return `${s} s`;
  if (s < 3600) return `${Math.floor(s / 60)} min`;
  return `${Math.floor(s / 3600)} h`;
}

/** Time since: "4 s ago", "3 min ago", "2 h ago". */
export function ago(ms: number): string {
  return ms < 1000 ? "just now" : `${span(ms)} ago`;
}

/** An interval setting: "every 2 minutes", "every 30 seconds". */
export function interval(secs: number): string {
  if (secs % 60 === 0) {
    const m = secs / 60;
    return m === 1 ? "every minute" : `every ${m} minutes`;
  }
  return secs === 1 ? "every second" : `every ${secs} seconds`;
}

/** Median of a list, or null for an empty one. */
export function median(xs: readonly number[]): number | null {
  if (xs.length === 0) return null;
  const s = [...xs].sort((a, b) => a - b);
  const mid = Math.floor(s.length / 2);
  return s.length % 2 === 1 ? s[mid]! : (s[mid - 1]! + s[mid]!) / 2;
}

/** "1 GPU", "72 GPUs". */
export function count(n: number, one: string, many: string): string {
  return `${n} ${n === 1 ? one : many}`;
}

/** UTC wall clock "14:03:07" for a Unix ms. */
export function utcClock(unixMs: number): string {
  const s = Math.floor(unixMs / 1000);
  const hh = Math.floor((s % 86400) / 3600);
  const mm = Math.floor((s % 3600) / 60);
  const ss = s % 60;
  return [hh, mm, ss].map((x) => String(x).padStart(2, "0")).join(":");
}

/** Time of day in hours as "06:30". */
export function timeOfDay(hours: number): string {
  const minutes = Math.round((((hours % 24) + 24) % 24) * 60) % (24 * 60);
  return `${String(Math.floor(minutes / 60)).padStart(2, "0")}:${String(minutes % 60).padStart(2, "0")}`;
}

/** A bit rate: "42 Mbit/s", "0.6 Mbit/s". */
export function mbps(bps: number): string {
  const m = bps / 1e6;
  return m < 10 ? `${m.toFixed(1)} Mbit/s` : `${Math.round(m)} Mbit/s`;
}
