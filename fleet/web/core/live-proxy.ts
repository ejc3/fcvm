// The coordinator under the page's own origin. On staging, Cloudflare Access gates the page
// host and the coordinator host as two applications, and an Access session cookie belongs
// to one host. A new browser session signs in to the page host only, so the page's
// cross-origin /live upgrade to the coordinator host is answered with Access's 302 to its
// login page, which a WebSocket cannot follow. The Worker that serves the page therefore
// forwards /live and /poster.jpg to the coordinator with a service token the coordinator's
// application admits, and the page connects to its own origin: one Access session.
//
// Without the token (local runs, the mock, a deploy before the token exists) nothing is
// forwarded and the page connects to FLEET_COORD_URL directly, as before.

/** The coordinator's paths the page uses. Nothing else is forwarded, /agent above all. */
export const PROXIED_PATHS: readonly string[] = ["/live", "/poster.jpg"];

export interface LiveProxy {
  /** The coordinator's origin, e.g. https://fleet-api-stage.fcvm.dev. */
  upstream: string;
  clientId: string;
  clientSecret: string;
}

/** The Worker's bindings, or process.env: only these three strings are read. */
export interface ProxyEnv {
  [binding: string]: unknown;
  FLEET_COORD_URL?: string;
  FLEET_ACCESS_CLIENT_ID?: string;
  FLEET_ACCESS_CLIENT_SECRET?: string;
}

/** The forwarding the Worker does, or null when it has no service token or no coordinator. */
export function liveProxy(env: ProxyEnv): LiveProxy | null {
  const id = env.FLEET_ACCESS_CLIENT_ID?.trim() ?? "";
  const secret = env.FLEET_ACCESS_CLIENT_SECRET?.trim() ?? "";
  if (id === "" || secret === "") return null;
  const m = /^(wss?):\/\/([^/?#]+)/i.exec(env.FLEET_COORD_URL ?? "");
  if (!m) return null;
  return { upstream: `${m[1]!.toLowerCase() === "wss" ? "https" : "http"}://${m[2]}`, clientId: id, clientSecret: secret };
}

/** Whether a request path is one the Worker forwards. */
export function isProxiedPath(pathname: string): boolean {
  return PROXIED_PATHS.includes(pathname);
}

/**
 * The viewer's address, for the coordinator's per-client limits. The Worker's subrequest
 * reaches Cloudflare's front door as a request of its own, so the coordinator sees the
 * Worker's address in CF-Connecting-IP for every viewer; it reads this header instead, and
 * only on a request Access admitted with a service token (src/fleet/coord/live.rs).
 */
export const VIEWER_ADDRESS_HEADER = "x-fleet-viewer-ip";

/**
 * Request headers the Worker drops: the viewer's Access credentials for the page host (the
 * coordinator's application has its own, the service token), any Access token or service
 * token a caller sent, the hop's own Host, and any viewer address a caller sent, which the
 * Worker sets itself.
 */
const DROPPED = new Set(["cookie", "host", "cf-access-jwt-assertion", "cf-access-token", "cf-access-client-id", "cf-access-client-secret", "x-real-ip", VIEWER_ADDRESS_HEADER]);

/**
 * The upstream URL and headers for a forwarded request. The rest of the headers pass
 * unchanged: Origin (the coordinator checks it against its allowed origins), Upgrade and the
 * Sec-WebSocket ones. The viewer's CF-Connecting-IP goes in VIEWER_ADDRESS_HEADER. With
 * `accessToken` (the coordinator application's JWT, which Access issued for the service
 * token on an earlier request) the Worker presents that instead of the service token.
 */
export function forwardedRequest(proxy: LiveProxy, path: string, search: string, headers: Iterable<[string, string]>, accessToken: string | null = null): { url: string; headers: [string, string][] } {
  const out: [string, string][] = [];
  let viewer: string | null = null;
  for (const [name, value] of headers) {
    const n = name.toLowerCase();
    if (n === "cf-connecting-ip") viewer = value;
    if (DROPPED.has(n)) continue;
    out.push([name, value]);
  }
  if (viewer !== null) out.push([VIEWER_ADDRESS_HEADER, viewer]);
  if (accessToken !== null) out.push(["cf-access-token", accessToken]);
  else out.push(["cf-access-client-id", proxy.clientId], ["cf-access-client-secret", proxy.clientSecret]);
  return { url: `${proxy.upstream}${path}${search}`, headers: out };
}

export interface AccessToken {
  token: string;
  /** Expiry, in seconds since the epoch, from the JWT's `exp`. */
  exp: number;
}

/** The coordinator application's JWT from an upstream response's Set-Cookie lines, or null. */
export function accessTokenFrom(setCookies: readonly string[]): AccessToken | null {
  for (const line of setCookies) {
    const m = /^\s*CF_Authorization=([A-Za-z0-9_-]+\.([A-Za-z0-9_-]+)\.[A-Za-z0-9_-]+)\s*(;|$)/.exec(line);
    if (!m) continue;
    const payload = b64urlText(m[2]!);
    if (payload === null) continue;
    try {
      const exp = (JSON.parse(payload) as { exp?: unknown }).exp;
      if (typeof exp === "number" && Number.isFinite(exp)) return { token: m[1]!, exp };
    } catch {
      // Not a JWT payload: not a token to keep.
    }
  }
  return null;
}

/** A kept token is used until a minute before it expires. */
export function tokenUsable(t: AccessToken | null, nowMs: number): t is AccessToken {
  return t !== null && t.exp * 1000 - 60_000 > nowMs;
}

/**
 * Whether an upstream answer may be Access refusing a kept token: a redirect to its login, or
 * 401/403. The coordinator's own 403 (a foreign Origin) also matches, which costs one retry
 * with the service token and is then passed on.
 */
export function accessRefused(status: number): boolean {
  return status === 401 || status === 403 || (status >= 300 && status < 400);
}

function b64urlText(s: string): string | null {
  const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
  let bits = 0;
  let acc = 0;
  const bytes: number[] = [];
  for (const c of s) {
    const v = alphabet.indexOf(c);
    if (v < 0) return null;
    acc = ((acc << 6) | v) & 0xffffff;
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      bytes.push((acc >> bits) & 0xff);
    }
  }
  // Byte to character: the claim read here, exp, is ASCII, as JSON's syntax is.
  return String.fromCharCode(...bytes);
}

/**
 * The /live URL the page connects to: its own origin when the Worker forwards, else
 * FLEET_COORD_URL. `proto` is the page request's scheme ("https" or "http").
 */
export function pageLiveUrl(env: ProxyEnv, host: string | null, proto: string | null): string {
  const direct = typeof env.FLEET_COORD_URL === "string" ? env.FLEET_COORD_URL : "";
  if (liveProxy(env) === null || host === null || host === "") return direct;
  return `${proto === "http" ? "ws" : "wss"}://${host}/live`;
}
