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

/** The text message that changes a live frame socket's height. */
export function heightMessage(height: number): string {
  return JSON.stringify({ h: Math.round(height) });
}

/** The host part of a ws:// or wss:// URL, for display. */
export function urlHost(url: string): string | null {
  const m = /^[a-z]+:\/\/(\[[^\]]+\]|[^/:?#]+)/i.exec(url);
  return m ? m[1]! : null;
}
