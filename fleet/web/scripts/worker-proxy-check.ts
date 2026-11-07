// The forwarding half of scripts/worker-smoke.sh: a stand-in coordinator that answers /live
// and /poster.jpg with the request headers it received, and the checks run against the
// built Worker that forwards to it (worker.ts, core/live-proxy.ts).
//
// The stand-in plays Access's part as the Worker meets it: the service token is admitted
// and answered with a CF_Authorization cookie (which must not reach the viewer); anything
// else gets a 302 to a login page.
//
//   node scripts/worker-proxy-check.ts serve <port>          the stand-in coordinator
//   node scripts/worker-proxy-check.ts check <worker port>   the checks; exit 1 on a failure

import { createServer, type IncomingHttpHeaders } from "node:http";
import { WebSocket as NodeWebSocket, WebSocketServer } from "ws";

const [mode, portArg] = process.argv.slice(2);
const port = Number(portArg);
const backendJwtHeader = "api-jwt";
const backendMarker = "coordinator-response";
if (!(port > 0)) throw new Error("usage: worker-proxy-check.ts serve|check <port>");

const b64url = (s: string) => Buffer.from(s).toString("base64url");

/** What the stand-in saw: the headers that decide whether forwarding is right. */
function seen(path: string, h: IncomingHttpHeaders, admittedBy: string): string {
  return JSON.stringify({
    path,
    admitted_by: admittedBy,
    origin: h.origin ?? null,
    cookie: h.cookie ?? null,
    jwt: h["cf-access-jwt-assertion"] ?? null,
    client_id: h["cf-access-client-id"] ?? null,
    viewer_ip: h["x-fleet-viewer-ip"] ?? null,
  });
}

if (mode === "serve") {
  /** Access, as far as the Worker meets it: the admitting credential, or null for a 302. */
  const admit = (h: IncomingHttpHeaders): { by: string; cookie: string } | null => {
    if (h["cf-access-client-id"] === "smoke-id" && h["cf-access-client-secret"] === "smoke-secret") {
      const jwt = `${b64url('{"alg":"RS256"}')}.${b64url(JSON.stringify({ exp: Math.floor(Date.now() / 1000) + 3600 }))}.sig`;
      return { by: "service-token", cookie: `CF_Authorization=${jwt}; Path=/; Secure; HttpOnly` };
    }
    return null;
  };
  const login = { location: "https://team.example.test/cdn-cgi/access/login" };
  const server = createServer((req, res) => {
    if (!req.url?.startsWith("/poster.jpg")) {
      res.writeHead(404).end("stand-in coordinator: not a coordinator path");
      return;
    }
    const a = admit(req.headers);
    if (a === null) {
      res.writeHead(302, login).end();
      return;
    }
    res.writeHead(200, {
      "content-type": "application/json",
      "set-cookie": a.cookie,
      "cf-access-jwt-assertion": backendJwtHeader,
      "x-coordinator-test": backendMarker,
    }).end(seen("/poster.jpg", req.headers, a.by));
  });
  const wss = new WebSocketServer({ noServer: true });
  server.on("upgrade", (req, socket, head) => {
    const a = req.url?.startsWith("/live") ? admit(req.headers) : null;
    if (a === null) {
      socket.end("HTTP/1.1 302 Found\r\nLocation: https://team.example.test/cdn-cgi/access/login\r\nContent-Length: 0\r\n\r\n");
      return;
    }
    wss.once("headers", (headers) => headers.push(
      `Set-Cookie: ${a.cookie}`,
      `CF-Access-Jwt-Assertion: ${backendJwtHeader}`,
      `X-Coordinator-Test: ${backendMarker}`,
    ));
    wss.handleUpgrade(req, socket, head, (ws) => ws.send(seen("/live", req.headers, a.by)));
  });
  server.listen(port, "127.0.0.1", () => console.log(`stand-in coordinator on ${port}`));
} else if (mode === "check") {
  const base = `127.0.0.1:${port}`;
  const failures: string[] = [];
  const expect = (what: string, ok: boolean, detail: string) => {
    console.log(`${ok ? "ok  " : "FAIL"} ${what}${ok ? "" : `: ${detail}`}`);
    if (!ok) failures.push(what);
  };
  const parse = (raw: string): Record<string, unknown> => {
    try {
      return JSON.parse(raw) as Record<string, unknown>;
    } catch {
      return {};
    }
  };
  // The viewer's own Access credentials and a viewer address it tries to set itself.
  const viewer = { Cookie: "CF_Authorization=viewer-session", "cf-access-jwt-assertion": "viewer-jwt", "cf-connecting-ip": "203.0.113.7", "x-fleet-viewer-ip": "198.51.100.66" };
  const poster = async (label: string) => {
    const r = await fetch(`http://${base}/poster.jpg`, { headers: viewer, signal: AbortSignal.timeout(10000) });
    const raw = await r.text();
    const s = parse(raw);
    expect(`${label}: /poster.jpg reached the coordinator`, r.status === 200 && s.path === "/poster.jpg", `${r.status} ${raw.slice(0, 200)}`);
    expect(`${label}: admitted by the Worker's service token`, s.admitted_by === "service-token", raw.slice(0, 200));
    expect(`${label}: Server-Timing gives the coordinator leg`, /upstream;dur=\d+/.test(r.headers.get("server-timing") ?? ""), r.headers.get("server-timing") ?? "none");
    expect(`${label}: drops the viewer's Access credentials`, s.cookie === null && s.jwt === null, raw.slice(0, 200));
    expect(`${label}: carries the viewer's own address, not one it sent`, s.viewer_ip === "203.0.113.7", raw.slice(0, 200));
    expect(`${label}: strips the coordinator's Access response credentials`, !r.headers.has("set-cookie") && !r.headers.has("cf-access-jwt-assertion"), "unexpected authentication header");
    expect(`${label}: keeps safe response headers`, r.headers.get("x-coordinator-test") === backendMarker, "missing test header");
  };

  const page = await fetch(`http://${base}/?mode=potato`, { signal: AbortSignal.timeout(30000) }).then((r) => r.text());
  expect("the page connects to its own origin", page.includes(`ws://${base}/live`), "no same-origin /live URL in the page");

  await poster("poster");

  // ws exposes the upgrade headers; a message afterwards proves the actual socket survives.
  const live = await new Promise<string>((resolve) => {
    const ws = new NodeWebSocket(`ws://${base}/live`);
    const timer = setTimeout(() => {
      resolve("timed out");
      ws.terminate();
    }, 10000);
    ws.once("upgrade", (response) => {
      expect("/live strips the coordinator's Access response credentials",
        !response.headers["set-cookie"] && !response.headers["cf-access-jwt-assertion"], "unexpected authentication header");
      expect("/live keeps safe upgrade headers", response.headers["x-coordinator-test"] === backendMarker, "missing test header");
      expect("/live carries Server-Timing", /upstream;dur=\d+/.test(String(response.headers["server-timing"] ?? "")), String(response.headers["server-timing"] ?? "none"));
    });
    ws.once("message", (data) => {
      clearTimeout(timer);
      resolve(String(data));
      ws.close();
    });
    ws.once("error", () => {
      clearTimeout(timer);
      resolve("socket error");
    });
  });
  const s = parse(live);
  expect("/live upgrade is relayed to the coordinator", s.path === "/live", live.slice(0, 200));
  expect("/live is admitted by the Worker's service token", s.admitted_by === "service-token", live.slice(0, 200));

  const agent = await fetch(`http://${base}/agent`, { signal: AbortSignal.timeout(10000) });
  const agentBody = await agent.text();
  expect("/agent is not forwarded", !agentBody.includes("smoke-id") && !agentBody.includes("stand-in coordinator"), `${agent.status} ${agentBody.slice(0, 120)}`);

  if (failures.length > 0) {
    console.error(`worker-proxy-check: ${failures.length} failed`);
    process.exit(1);
  }
  console.log("worker-proxy-check: forwarding ok");
} else {
  throw new Error("usage: worker-proxy-check.ts serve|check <port>");
}
