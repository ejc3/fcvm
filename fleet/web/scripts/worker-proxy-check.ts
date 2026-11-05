// The forwarding half of scripts/worker-smoke.sh: a stand-in coordinator that answers /live
// and /poster.jpg with the request headers it received, and the checks run against the
// built Worker that forwards to it (worker.ts, core/live-proxy.ts).
//
//   node scripts/worker-proxy-check.ts serve <port>          the stand-in coordinator
//   node scripts/worker-proxy-check.ts check <worker port>   the checks; exit 1 on a failure

import { createServer, type IncomingHttpHeaders } from "node:http";
import { WebSocketServer } from "ws";

const [mode, portArg] = process.argv.slice(2);
const port = Number(portArg);
if (!(port > 0)) throw new Error("usage: worker-proxy-check.ts serve|check <port>");

/** What the stand-in saw: the headers that decide whether forwarding is right. */
function seen(path: string, h: IncomingHttpHeaders): string {
  return JSON.stringify({
    path,
    origin: h.origin ?? null,
    cookie: h.cookie ?? null,
    jwt: h["cf-access-jwt-assertion"] ?? null,
    client_id: h["cf-access-client-id"] ?? null,
    client_secret: h["cf-access-client-secret"] ?? null,
    real_ip: h["x-real-ip"] ?? null,
  });
}

if (mode === "serve") {
  const server = createServer((req, res) => {
    if (req.url?.startsWith("/poster.jpg")) {
      res.writeHead(200, { "content-type": "application/json" }).end(seen("/poster.jpg", req.headers));
    } else {
      res.writeHead(404).end("stand-in coordinator: not a coordinator path");
    }
  });
  const wss = new WebSocketServer({ noServer: true });
  server.on("upgrade", (req, socket, head) => {
    if (!req.url?.startsWith("/live")) {
      socket.destroy();
      return;
    }
    wss.handleUpgrade(req, socket, head, (ws) => ws.send(seen("/live", req.headers)));
  });
  server.listen(port, "127.0.0.1", () => console.log(`stand-in coordinator on ${port}`));
} else if (mode === "check") {
  const base = `127.0.0.1:${port}`;
  const failures: string[] = [];
  const expect = (what: string, ok: boolean, detail: string) => {
    console.log(`${ok ? "ok  " : "FAIL"} ${what}${ok ? "" : `: ${detail}`}`);
    if (!ok) failures.push(what);
  };
  const viewer = { Cookie: "CF_Authorization=viewer-session", "cf-access-jwt-assertion": "viewer-jwt", "cf-connecting-ip": "203.0.113.7" };
  const forwarded = (raw: string, path: string) => {
    let s: Record<string, unknown> = {};
    try {
      s = JSON.parse(raw) as Record<string, unknown>;
    } catch {
      // Not the stand-in's answer: every check below fails and prints it.
    }
    expect(`${path} reached the coordinator`, s.path === path, raw.slice(0, 200));
    expect(`${path} carries the Worker's service token`, s.client_id === "smoke-id" && s.client_secret === "smoke-secret", raw.slice(0, 200));
    expect(`${path} drops the viewer's Access credentials`, s.cookie === null && s.jwt === null, raw.slice(0, 200));
    expect(`${path} carries the viewer's address in x-real-ip`, s.real_ip === "203.0.113.7", raw.slice(0, 200));
    return s;
  };

  const page = await fetch(`http://${base}/?mode=potato`, { signal: AbortSignal.timeout(30000) }).then((r) => r.text());
  expect("the page connects to its own origin", page.includes(`ws://${base}/live`), "no same-origin /live URL in the page");

  const poster = await fetch(`http://${base}/poster.jpg`, { headers: viewer, signal: AbortSignal.timeout(10000) }).then((r) => r.text());
  forwarded(poster, "/poster.jpg");

  // Node's WebSocket sends no cookie or custom headers, so the viewer's headers ride on the
  // HTTP checks above; this one proves the upgrade itself is relayed and Origin passes.
  const live = await new Promise<string>((resolve) => {
    const ws = new WebSocket(`ws://${base}/live`);
    const timer = setTimeout(() => resolve("timed out"), 10000);
    ws.onmessage = (e) => {
      clearTimeout(timer);
      resolve(String(e.data));
      ws.close();
    };
    ws.onerror = () => {
      clearTimeout(timer);
      resolve("socket error");
    };
  });
  const s = (() => {
    try {
      return JSON.parse(live) as Record<string, unknown>;
    } catch {
      return {};
    }
  })();
  expect("/live upgrade is relayed to the coordinator", s.path === "/live", live.slice(0, 200));
  expect("/live carries the Worker's service token", s.client_id === "smoke-id", live.slice(0, 200));

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
