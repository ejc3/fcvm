import { createServer, type Server } from "node:http";
import type { AddressInfo, Socket } from "node:net";
import { afterAll, describe, expect, it } from "vitest";
import { WebSocketServer } from "ws";
import { WallClient } from "../../app/_wall/wall-client.ts";
import { afterLiveFailure, FIRST_LIVE_TRY, LIVE_OPEN_DEADLINE_MS } from "../net.ts";

describe("afterLiveFailure", () => {
  it("tries the next URL at once until every URL has failed in a row", () => {
    const one = afterLiveFailure(FIRST_LIVE_TRY, 2);
    expect(one).toEqual({ next: { index: 1, failedInRow: 1 }, now: true });
    expect(afterLiveFailure(one.next, 2)).toEqual({ next: FIRST_LIVE_TRY, now: false });
  });

  it("waits for the reconnect delay with one URL, as before", () => {
    expect(afterLiveFailure(FIRST_LIVE_TRY, 1)).toEqual({ next: FIRST_LIVE_TRY, now: false });
  });

  it("starts from a URL that worked when it fails later", () => {
    expect(afterLiveFailure({ index: 1, failedInRow: 0 }, 2)).toEqual({ next: { index: 0, failedInRow: 1 }, now: true });
  });
});

/** A /live stand-in: "ok" sends a hello, "refuse" answers the upgrade with Access's 302, "hang" never answers. */
async function standIn(mode: "ok" | "refuse" | "hang"): Promise<{ url: string; upgrades: () => number; close: () => Promise<void> }> {
  let upgrades = 0;
  const sockets = new Set<Socket>();
  const server: Server = createServer((_req, res) => res.writeHead(404).end());
  server.on("connection", (s) => {
    sockets.add(s);
    s.on("close", () => sockets.delete(s));
  });
  const wss = new WebSocketServer({ noServer: true });
  server.on("upgrade", (req, socket, head) => {
    upgrades++;
    if (mode === "refuse") {
      socket.end("HTTP/1.1 302 Found\r\nLocation: https://team.example.test/cdn-cgi/access/login\r\nContent-Length: 0\r\n\r\n");
      return;
    }
    if (mode === "hang") return;
    wss.handleUpgrade(req, socket, head, (ws) => ws.send(JSON.stringify({ type: "hello", viewer: "v1" })));
  });
  await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
  const { port } = server.address() as AddressInfo;
  return {
    url: `ws://127.0.0.1:${port}/live`,
    upgrades: () => upgrades,
    close: async () => {
      for (const s of sockets) s.destroy();
      wss.close();
      await new Promise<void>((r) => server.close(() => r()));
    },
  };
}

async function openOn(client: WallClient, ms: number): Promise<number> {
  const t0 = performance.now();
  while (client.getState().connection !== "open") {
    if (performance.now() - t0 > ms) throw new Error(`no socket open in ${ms} ms (state ${client.getState().connection})`);
    await new Promise((r) => setTimeout(r, 10));
  }
  return performance.now() - t0;
}

describe("WallClient tries the direct URL first", () => {
  const cleanup: (() => Promise<void> | void)[] = [];
  afterAll(async () => {
    for (const c of cleanup) await c();
  });

  it("falls back to the page's origin at once when the direct URL is refused", async () => {
    const direct = await standIn("refuse");
    const proxied = await standIn("ok");
    cleanup.push(direct.close, proxied.close);
    const client = new WallClient(proxied.url, undefined, direct.url);
    cleanup.push(() => client.stop());
    client.start();
    const ms = await openOn(client, 1000);
    expect(client.url).toBe(proxied.url);
    expect(direct.upgrades()).toBe(1);
    // No reconnect delay between the refusal and the fallback (the first delay is 500 ms).
    expect(ms).toBeLessThan(400);
  });

  it("uses the direct URL when it opens, and never asks the page's origin", async () => {
    const direct = await standIn("ok");
    const proxied = await standIn("ok");
    cleanup.push(direct.close, proxied.close);
    const client = new WallClient(proxied.url, undefined, direct.url);
    cleanup.push(() => client.stop());
    client.start();
    await openOn(client, 1000);
    expect(client.url).toBe(direct.url);
    expect(proxied.upgrades()).toBe(0);
  });

  it("does not wait past the deadline for a direct URL that never answers", async () => {
    const direct = await standIn("hang");
    const proxied = await standIn("ok");
    cleanup.push(direct.close, proxied.close);
    const client = new WallClient(proxied.url, undefined, direct.url);
    cleanup.push(() => client.stop());
    client.start();
    const ms = await openOn(client, LIVE_OPEN_DEADLINE_MS + 1500);
    expect(client.url).toBe(proxied.url);
    expect(ms).toBeGreaterThanOrEqual(LIVE_OPEN_DEADLINE_MS - 50);
  });

  it("reconnects to the direct URL that worked", async () => {
    const direct = await standIn("ok");
    const proxied = await standIn("ok");
    cleanup.push(direct.close, proxied.close);
    const client = new WallClient(proxied.url, undefined, direct.url);
    cleanup.push(() => client.stop());
    client.start();
    await openOn(client, 1000);
    // The coordinator restarts: the open socket closes from its side.
    (client as unknown as { ws: WebSocket }).ws.close();
    while (client.getState().connection === "open") await new Promise((r) => setTimeout(r, 10));
    await openOn(client, 2000);
    expect(client.url).toBe(direct.url);
    expect(direct.upgrades()).toBe(2);
    expect(proxied.upgrades()).toBe(0);
  });
});
