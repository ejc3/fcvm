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
