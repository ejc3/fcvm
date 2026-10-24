// Connection rules shared by the coordinator socket and the frame sockets.

/** Delay before reconnect attempt `attempt` (0-based): 0.5 s doubling, capped at 10 s. */
export function reconnectDelayMs(attempt: number): number {
  return Math.min(10_000, 500 * 2 ** Math.max(0, Math.min(attempt, 10)));
}

/** A frame socket URL: frame_url with h set, and q when it is not the default. */
export function frameSocketUrl(frameUrl: string, height: number, quality: "hi" | "std" | "lo" = "std"): string {
  const [base, query = ""] = frameUrl.split("?", 2) as [string, string?];
  const kept = query
    .split("&")
    .filter((p) => p !== "" && !p.startsWith("h=") && !p.startsWith("q="));
  kept.push(`h=${Math.round(height)}`);
  if (quality !== "std") kept.push(`q=${quality}`);
  return `${base}?${kept.join("&")}`;
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
 * How long the IPv6 probe waits for a frame socket to open. A working IPv6 connect to a
 * staging tile measured 177 to 188 ms from a browser (10 of 10, 2026-10-24); a refused
 * or unreachable one fails sooner or never answers. 1.5 s is about 8x the working time.
 */
export const FAMILY_PROBE_MS = 1500;

/** How many tiles the probe tries at once: one tile can be down without IPv6 being down. */
export const FAMILY_PROBE_TILES = 3;

/**
 * How many probes fail before the page settles on IPv4, and the wait between them. A fast
 * failure does not tell "no IPv6" (no route) from "not accepting yet" (a renderer that has
 * just reported running, as at the end of a wake), so one failed probe is not enough.
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
 * What the probe opens: the IPv6 URLs of up to FAMILY_PROBE_TILES running tiles. Empty when
 * no tile has one yet. When only one family has URLs there is nothing to choose: `v4Only`
 * (tiles have IPv4 URLs and none an IPv6 one) and `v6Only` (the other way round, as with a
 * coordinator that publishes no IPv4 names) settle the family without a probe.
 */
export function familyProbeTargets(tiles: readonly { phase: string; frame_url: string | null; frame_url_v4: string | null }[]): {
  urls: string[];
  v4Only: boolean;
  v6Only: boolean;
} {
  const running = tiles.filter((t) => t.phase === "running");
  const urls = running.flatMap((t) => (t.frame_url === null ? [] : [t.frame_url])).slice(0, FAMILY_PROBE_TILES);
  const anyV4 = running.some((t) => t.frame_url_v4 !== null);
  return { urls, v4Only: urls.length === 0 && anyV4, v6Only: urls.length > 0 && !anyV4 };
}
