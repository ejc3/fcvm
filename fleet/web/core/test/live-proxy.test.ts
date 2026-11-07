import { describe, expect, it } from "vitest";
import { forwardedRequest, isProxiedPath, liveProxy, pageDirectLiveUrl, pageLiveUrl, VIEWER_ADDRESS_HEADER } from "../live-proxy.ts";

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

describe("pageDirectLiveUrl", () => {
  it("splits browser direct access from the token-bearing Worker fallback", () => {
    const separate = { ...env, FLEET_DIRECT_COORD_URL: "wss://viewer.example.test/live" };
    expect(pageDirectLiveUrl(separate)).toBe(separate.FLEET_DIRECT_COORD_URL);
    expect(pageLiveUrl(separate, "stage.example.test", "https")).toBe("wss://stage.example.test/live");
    const upstream = forwardedRequest(liveProxy(separate)!, "/live", "", []);
    expect(upstream.url).toBe("https://coord.example.test/live");
    expect(upstream.headers).toContainEqual(["cf-access-client-secret", "secret-1"]);
  });

  it("keeps the old direct path if unconfigured and never creates a fallback without the Worker token", () => {
    expect(pageDirectLiveUrl(env)).toBe(env.FLEET_COORD_URL);
    expect(pageDirectLiveUrl({ FLEET_COORD_URL: env.FLEET_COORD_URL, FLEET_DIRECT_COORD_URL: "wss://viewer.example.test/live" })).toBeNull();
  });

  it("uses only an exact secure viewer socket for the optional override", () => {
    for (const bad of ["not a URL", "https://viewer.example.test/live", "ws://viewer.example.test/live",
      "wss://viewer.example.test/agent", "wss://viewer.example.test/live?token=test", "wss://user@viewer.example.test/live"])
      expect(pageDirectLiveUrl({ ...env, FLEET_DIRECT_COORD_URL: bad })).toBeNull();
  });
});
