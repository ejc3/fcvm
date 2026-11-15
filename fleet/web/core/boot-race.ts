// The boot-race page (/boot-race): one recorded campaign, read from app/boot-race/summary.json, which
// bench/boot-race/summary.py writes from the published campaign page's data. Recorded results, never live.
// parseBootRaceSummary refuses a summary that could let the page show a number without its context: a
// missing summary, a missing campaign field, or a lane without its configuration.

export interface BootRaceLane {
  readonly id: string;
  readonly name: string;
  readonly start: string;
  readonly config: string;
  readonly check: string;
  readonly n: number;
  readonly failures: number;
  readonly median_ms: number;
  readonly p95_ms: number;
  readonly p99_ms: number;
  readonly min_ms: number;
  readonly max_ms: number;
}

export interface BootRaceNotRun {
  readonly id: string;
  readonly name: string;
  readonly why: string;
}

export interface BootRaceSummary {
  readonly recorded: true;
  readonly campaign: string;
  readonly window_utc: { readonly first_sample: string; readonly last_sample: string };
  readonly rounds: number;
  readonly fcvm_commit: string;
  readonly region: string;
  readonly campaign_host: string;
  readonly host_load: string;
  readonly records: { readonly repository: string; readonly commit: string; readonly url: string };
  readonly page_url: string;
  readonly lanes: readonly BootRaceLane[];
  readonly not_run: readonly BootRaceNotRun[];
}

function fail(what: string): never {
  throw new Error(`boot-race summary: ${what} (regenerate it with bench/boot-race/summary.py)`);
}

function text(o: Record<string, unknown>, key: string, where: string): string {
  const v = o[key];
  if (typeof v !== "string" || v.trim() === "") fail(`${where} has no ${key}`);
  return v;
}

function num(o: Record<string, unknown>, key: string, where: string): number {
  const v = o[key];
  if (typeof v !== "number" || !Number.isFinite(v)) fail(`${where} has no ${key}`);
  return v;
}

function record(v: unknown, where: string): Record<string, unknown> {
  if (typeof v !== "object" || v === null || Array.isArray(v)) fail(`${where} is missing`);
  return v as Record<string, unknown>;
}

/** The summary, checked: every field the page states is present, and every lane names its configuration. */
export function parseBootRaceSummary(raw: unknown): BootRaceSummary {
  const s = record(raw, "the summary");
  if (s.recorded !== true) fail("it is not marked as recorded results");
  const win = record(s.window_utc, "window_utc");
  const rec = record(s.records, "records");
  if (!Array.isArray(s.lanes) || s.lanes.length === 0) fail("it has no lanes");
  const lanes = s.lanes.map((l: unknown, i: number): BootRaceLane => {
    const o = record(l, `lane ${i}`);
    const where = `lane ${typeof o.id === "string" ? o.id : i}`;
    return {
      id: text(o, "id", where), name: text(o, "name", where), start: text(o, "start", where),
      config: text(o, "config", where), check: typeof o.check === "string" ? o.check : "",
      n: num(o, "n", where), failures: num(o, "failures", where),
      median_ms: num(o, "median_ms", where), p95_ms: num(o, "p95_ms", where), p99_ms: num(o, "p99_ms", where),
      min_ms: num(o, "min_ms", where), max_ms: num(o, "max_ms", where),
    };
  });
  const notRun = Array.isArray(s.not_run) ? s.not_run : fail("it has no not_run list");
  return {
    recorded: true,
    campaign: text(s, "campaign", "the summary"),
    window_utc: { first_sample: text(win, "first_sample", "window_utc"), last_sample: text(win, "last_sample", "window_utc") },
    rounds: num(s, "rounds", "the summary"),
    fcvm_commit: text(s, "fcvm_commit", "the summary"),
    region: text(s, "region", "the summary"),
    campaign_host: text(s, "campaign_host", "the summary"),
    host_load: typeof s.host_load === "string" ? s.host_load : "",
    records: { repository: text(rec, "repository", "records"), commit: text(rec, "commit", "records"), url: text(rec, "url", "records") },
    page_url: text(s, "page_url", "the summary"),
    lanes,
    not_run: notRun.map((n: unknown, i: number) => {
      const o = record(n, `not_run ${i}`);
      return { id: text(o, "id", `not_run ${i}`), name: text(o, "name", `not_run ${i}`), why: text(o, "why", `not_run ${i}`) };
    }),
  };
}

/** A lane time as the published page writes it: "63.7 ms" under 100 ms, "82 ms" under a second, then "1.49 s". */
export function laneTime(ms: number): string {
  if (ms < 100) return `${ms.toFixed(1)} ms`;
  if (ms < 1000) return `${ms.toFixed(0)} ms`;
  return `${(ms / 1000).toFixed(ms < 10_000 ? 2 : 1)} s`;
}

/** "2026-11-14T15:05:37Z" as "2026-11-14 15:05:37 UTC". */
export function utc(iso: string): string {
  return iso.replace("T", " ").replace(/Z$/, " UTC");
}
