// Connection rules shared by the coordinator socket and the frame sockets.

/** Delay before reconnect attempt `attempt` (0-based): 0.5 s doubling, capped at 10 s. */
export function reconnectDelayMs(attempt: number): number {
  return Math.min(10_000, 500 * 2 ** Math.max(0, Math.min(attempt, 10)));
}

/**
 * Which of the page's /live URLs a socket uses: `index` into them, and how many have failed
 * in a row without opening. The page lists its URLs fastest first: on staging the
 * coordinator's own host, which needs the browser to hold an Access session there, then the
 * page's origin, which its Worker forwards (core/live-proxy.ts).
 */
export interface LiveTry {
  index: number;
  failedInRow: number;
}

export const FIRST_LIVE_TRY: LiveTry = { index: 0, failedInRow: 0 };

/**
 * How long a socket that is not the last URL may take to open before the next URL is tried.
 * A direct socket to the staging coordinator opened in 196 to 244 ms (median of 6, three
 * runs, 2026-11-07), and one without an Access session fails with Access's 302 in about
 * 40 ms; 2 s is eight times the slower median.
 */
export const LIVE_OPEN_DEADLINE_MS = 2000;

/**
 * After a socket closed without opening: the next URL, tried at once, until every URL has
 * failed in a row, when the next try waits for the reconnect delay and starts over at the
 * first. A socket that opened and closed later reconnects to the URL that worked.
 */
export function afterLiveFailure(t: LiveTry, count: number): { next: LiveTry; now: boolean } {
  const failedInRow = t.failedInRow + 1;
  if (failedInRow >= count) return { next: FIRST_LIVE_TRY, now: false };
  return { next: { index: (t.index + 1) % count, failedInRow }, now: true };
}

/**
 * How long frame socket attempt `attempt` (0-based, counting failures since the stream last
 * decoded a frame) may take to open before it is closed and retried: 5 s, then 10 s, then 20 s.
 * A connect whose packets are dropped is never refused, and Chrome's own connect timeout is
 * minutes: a Windows sandbox's IPv4 port that a security group dropped left its tile black with
 * nothing to say why (staging, 2026-11-13). A working frame socket opened in 186 to 243 ms over
 * either family from a browser on a dev box (staging GPU tiles, 2026-11-13), so 5 s is twenty
 * times the slower; the doubling gives a slow link more time on each try.
 */
export function frameOpenDeadlineMs(attempt: number): number {
  return 5000 * 2 ** Math.max(0, Math.min(attempt, 2));
}

/** A frame socket URL: frame_url with h set, and q when it is not the default. */
export function frameSocketUrl(frameUrl: string, height: number, quality: "hi" | "std" | "lo" = "std"): string {
  const [base, query = ""] = frameUrl.split("?", 2) as [string, string?];
  const kept = query.split("&").filter((p) => p !== "" && !p.startsWith("h=") && !p.startsWith("q="));
  kept.push(`h=${Math.round(height)}`);
  if (quality !== "std") kept.push(`q=${quality}`);
  return `${base}?${kept.join("&")}`;
}

/** The GPU mirror's socket on a phone streamer (fleet/PROTOCOL.md, "Phone streamer", GET /mirror). */
export function mirrorSocketUrl(frameUrl: string): string {
  const base = frameUrl.split("?", 1)[0]!;
  return base.replace(/\/frames$/, "") + "/mirror";
}

/**
 * A frame socket's first message when the page holds a direct token (PROTOCOL.md, "Access"):
 * the renderer checks it before it streams. It is never in the URL, which proxies and access
 * logs keep.
 */
export function admitMessage(token: string): string {
  return JSON.stringify({ admit: token });
}

/**
 * Whether a socket opened on `a` already serves what `b` asks for, once the
 * height is set with a message. The renderer reads every other query parameter
 * (q, and any the frame URL carries) at connection time only.
 */
export function sameStreamAtAnyHeight(a: string, b: string): boolean {
  const strip = (u: string): string => {
    const [base, query = ""] = u.split("?", 2) as [string, string?];
    return `${base}?${query.split("&").filter((p) => p !== "" && !p.startsWith("h=")).join("&")}`;
  };
  return strip(a) === strip(b);
}

/** The text message that changes a live frame socket's height. */
export function heightMessage(height: number): string {
  return JSON.stringify({ h: Math.round(height) });
}

/** The host part of a ws:// or wss:// URL, for display. */
export function urlHost(url: string): string | null {
  const m = /^[a-z]+:\/\/(\[[^\]]+\]|[^/:?#]+)/i.exec(url);
  return m ? m[1]! : null;
}

/** The coordinator's poster: the last whole image as a JPEG, on the /live socket's origin. */
export function posterUrl(liveUrl: string): string | null {
  const m = /^(wss?):\/\/([^/?#]+)/i.exec(liveUrl);
  if (!m) return null;
  return `${m[1]!.toLowerCase() === "wss" ? "https" : "http"}://${m[2]}/poster.jpg`;
}

/**
 * The address family a viewer streams every tile over. One family for both halves of the
 * race, so the fcvm and EC2 tiles are measured over the same kind of path.
 */
export type Family = "v6" | "v4";

/**
 * How the family was chosen: a socket of it opened first in the probe ("race"), only one
 * family has URLs ("one-family"), or no socket of either family opened in
 * FAMILY_PROBE_ATTEMPTS probes ("no-answer", which settles on IPv4).
 */
export type FamilyVia = "race" | "one-family" | "no-answer";

/**
 * How long one probe waits for a frame socket of either family to open. A working IPv6
 * connect to a staging tile measured 177 to 188 ms from a browser (10 of 10, 2026-10-24); a
 * refused or unreachable one fails sooner or never answers. 1.5 s is about 8x the working
 * time.
 */
export const FAMILY_PROBE_MS = 1500;

/**
 * IPv6's head start in a probe (RFC 8305's connection attempt delay): the IPv6 sockets open
 * first, and the IPv4 ones this much later, or at once when every IPv6 socket has failed.
 * The first socket of either family to open sets the family, so IPv6 wins whenever it
 * connects within its head start or before IPv4 does.
 */
export const FAMILY_V6_HEAD_START_MS = 250;

/** How many tiles the probe tries at once: one tile can be down without IPv6 being down. */
export const FAMILY_PROBE_TILES = 3;

/**
 * How many probes fail before the page settles on IPv4, and the wait between them. A probe
 * fails when no socket of either family opens. A fast failure does not tell "no route" from
 * "not accepting yet" (a renderer that has just reported running, as at the end of a wake),
 * so one failed probe is not enough.
 */
export const FAMILY_PROBE_ATTEMPTS = 3;
export const FAMILY_PROBE_RETRY_MS = 1000;

/**
 * A stream's base URL in a family (a tile's, an aggregate's or a borrowed GPU's: every
 * `frame_url` has a `frame_url_v4` sibling): null while the family is unknown or the URL is.
 */
export function streamUrl(x: { frame_url: string | null; frame_url_v4: string | null }, family: Family | null): string | null {
  if (family === null) return null;
  return family === "v6" ? x.frame_url : x.frame_url_v4;
}

/**
 * What the probe opens: the IPv6 URLs (`urls`) and IPv4 URLs (`v4Urls`) of up to
 * FAMILY_PROBE_TILES running tiles each. `urls` is empty when no tile has one yet. When only one family has URLs there is nothing to choose: `v4Only`
 * (tiles have IPv4 URLs and none an IPv6 one) and `v6Only` (the other way round, as with a
 * coordinator that publishes no IPv4 names) settle the family without a probe.
 */
export function familyProbeTargets(tiles: readonly { phase: string; frame_url: string | null; frame_url_v4: string | null }[]): {
  urls: string[];
  v4Urls: string[];
  v4Only: boolean;
  v6Only: boolean;
} {
  const running = tiles.filter((t) => t.phase === "running");
  const urls = running.flatMap((t) => (t.frame_url === null ? [] : [t.frame_url])).slice(0, FAMILY_PROBE_TILES);
  const v4Urls = running.flatMap((t) => (t.frame_url_v4 === null ? [] : [t.frame_url_v4])).slice(0, FAMILY_PROBE_TILES);
  return { urls, v4Urls, v4Only: urls.length === 0 && v4Urls.length > 0, v6Only: urls.length > 0 && v4Urls.length === 0 };
}
