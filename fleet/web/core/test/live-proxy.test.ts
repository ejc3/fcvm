import { describe, expect, it } from "vitest";
import { accessRefused, accessTokenFrom, forwardedRequest, isProxiedPath, liveProxy, pageLiveUrl, tokenUsable, VIEWER_ADDRESS_HEADER } from "../live-proxy.ts";

const env = { FLEET_COORD_URL: "wss://coord.example.test/live", FLEET_ACCESS_CLIENT_ID: "id-1", FLEET_ACCESS_CLIENT_SECRET: "secret-1" };

describe("liveProxy", () => {
  it("forwards only with both halves of the service token and a coordinator URL", () => {
    expect(liveProxy(env)).toEqual({ upstream: "https://coord.example.test", clientId: "id-1", clientSecret: "secret-1" });
    expect(liveProxy({ ...env, FLEET_ACCESS_CLIENT_ID: "" })).toBeNull();
    expect(liveProxy({ ...env, FLEET_ACCESS_CLIENT_SECRET: " " })).toBeNull();
    expect(liveProxy({ FLEET_COORD_URL: env.FLEET_COORD_URL })).toBeNull();
    expect(liveProxy({ ...env, FLEET_COORD_URL: "" })).toBeNull();
    expect(liveProxy({ ...env, FLEET_COORD_URL: "ws://127.0.0.1:9000/live" })?.upstream).toBe("http://127.0.0.1:9000");
  });

  it("forwards the page's coordinator paths and nothing else", () => {
    expect(isProxiedPath("/live")).toBe(true);
    expect(isProxiedPath("/poster.jpg")).toBe(true);
    for (const p of ["/agent", "/healthz", "/", "/live/", "/livex", "/media/wall-reel.mp4"]) expect(isProxiedPath(p)).toBe(false);
  });
});

describe("forwardedRequest", () => {
  const proxy = liveProxy(env)!;
  const incoming: [string, string][] = [
    ["Origin", "https://stage.example.test"],
    ["Upgrade", "websocket"],
    ["Sec-WebSocket-Key", "k"],
    ["Cookie", "CF_Authorization=page-session"],
    ["cf-access-jwt-assertion", "page-jwt"],
    ["CF-Access-Client-Id", "caller-id"],
    ["CF-Access-Client-Secret", "caller-secret"],
    ["Host", "stage.example.test"],
    ["x-real-ip", "198.51.100.9"],
    [VIEWER_ADDRESS_HEADER, "198.51.100.66"],
    ["cf-connecting-ip", "203.0.113.7"],
  ];
  const { url, headers } = forwardedRequest(proxy, "/live", "?a=1", incoming);
  const get = (n: string) => headers.filter(([k]) => k.toLowerCase() === n).map(([, v]) => v);

  it("keeps the path, query, Origin and WebSocket headers", () => {
    expect(url).toBe("https://coord.example.test/live?a=1");
    expect(get("origin")).toEqual(["https://stage.example.test"]);
    expect(get("upgrade")).toEqual(["websocket"]);
    expect(get("sec-websocket-key")).toEqual(["k"]);
  });

  it("replaces the viewer's Access credentials with the Worker's service token", () => {
    expect(get("cookie")).toEqual([]);
    expect(get("cf-access-jwt-assertion")).toEqual([]);
    expect(get("host")).toEqual([]);
    expect(get("cf-access-client-id")).toEqual(["id-1"]);
    expect(get("cf-access-client-secret")).toEqual(["secret-1"]);
  });

  it("carries the viewer's address in its own header, never a caller's", () => {
    expect(get(VIEWER_ADDRESS_HEADER)).toEqual(["203.0.113.7"]);
    expect(get("x-real-ip")).toEqual([]);
    expect(forwardedRequest(proxy, "/live", "", [[VIEWER_ADDRESS_HEADER, "198.51.100.9"]]).headers.filter(([k]) => k === VIEWER_ADDRESS_HEADER)).toEqual([]);
  });

  it("presents a kept Access token instead of the service token", () => {
    const kept = forwardedRequest(proxy, "/live", "", incoming, "kept.jwt.value").headers;
    const has = (n: string) => kept.filter(([k]) => k.toLowerCase() === n).map(([, v]) => v);
    expect(has("cf-access-token")).toEqual(["kept.jwt.value"]);
    expect(has("cf-access-client-id")).toEqual([]);
    expect(has("cf-access-client-secret")).toEqual([]);
    expect(get("cf-access-token")).toEqual([]);
  });
});

describe("the kept Access token", () => {
  const b64 = (s: string) => Buffer.from(s).toString("base64url");
  const jwt = `${b64('{"alg":"RS256"}')}.${b64('{"exp":1900000000,"type":"app"}')}.c2ln`;

  it("is read from the coordinator application's CF_Authorization cookie", () => {
    expect(accessTokenFrom([`CF_Authorization=${jwt}; Path=/; Secure; HttpOnly`])).toEqual({ token: jwt, exp: 1900000000 });
    expect(accessTokenFrom(["other=1", `CF_Authorization=${jwt}`])).toEqual({ token: jwt, exp: 1900000000 });
    expect(accessTokenFrom(["CF_AppSession=abc; Path=/"])).toBeNull();
    expect(accessTokenFrom(["CF_Authorization=not-a-jwt; Path=/"])).toBeNull();
    expect(accessTokenFrom([`CF_Authorization=${b64("{}")}.${b64('{"type":"app"}')}.c2ln`])).toBeNull();
    expect(accessTokenFrom([])).toBeNull();
  });

  it("is used until a minute before it expires", () => {
    const t = { token: jwt, exp: 1900000000 };
    expect(tokenUsable(t, 1900000000_000 - 61_000)).toBe(true);
    expect(tokenUsable(t, 1900000000_000 - 59_000)).toBe(false);
    expect(tokenUsable(null, 0)).toBe(false);
  });

  it("is dropped on a redirect, 401 or 403, never on a 101 or 200", () => {
    for (const s of [301, 302, 307, 401, 403]) expect(accessRefused(s)).toBe(true);
    for (const s of [101, 200, 404, 500]) expect(accessRefused(s)).toBe(false);
  });
});

describe("pageLiveUrl", () => {
  it("is the page's own origin when the Worker forwards", () => {
    expect(pageLiveUrl(env, "stage.example.test", "https")).toBe("wss://stage.example.test/live");
    expect(pageLiveUrl(env, "stage.example.test", null)).toBe("wss://stage.example.test/live");
    expect(pageLiveUrl(env, "127.0.0.1:24110", "http")).toBe("ws://127.0.0.1:24110/live");
  });

  it("is FLEET_COORD_URL when it does not", () => {
    expect(pageLiveUrl({ FLEET_COORD_URL: env.FLEET_COORD_URL }, "stage.example.test", "https")).toBe(env.FLEET_COORD_URL);
    expect(pageLiveUrl(env, null, "https")).toBe(env.FLEET_COORD_URL);
    expect(pageLiveUrl({}, "stage.example.test", "https")).toBe("");
  });
});
