// The page's address family probe (app/_wall/wall-client.ts), driven with fake sockets
// and fake timers: how long after the first snapshot every tile can start streaming.
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { WallClient } from "../../app/_wall/wall-client.ts";
import { EPOCH, fleet, snapshotText } from "./fixtures.ts";

/** What a host does with a frame socket: open after `ms`, fail after `ms`, or never answer. */
type Answer = { open: number } | { fail: number } | "silent";

class FakeSocket {
  static answers = new Map<string, Answer>();
  static opened: string[] = [];
  onopen: (() => void) | null = null;
  onerror: (() => void) | null = null;
  onclose: (() => void) | null = null;
  readonly url: string;
  constructor(url: string) {
    this.url = url;
    FakeSocket.opened.push(url);
    const a = FakeSocket.answers.get(new URL(url).hostname) ?? "silent";
    if (a === "silent") return;
    if ("open" in a) setTimeout(() => this.onopen?.(), a.open);
    else
      setTimeout(() => {
        this.onerror?.();
        this.onclose?.();
      }, a.fail);
  }
  close(): void {}
}

// fixtures.fleet(): IPv6 URLs on example-host, IPv4 URLs on example-host-v4.
const V6 = "example-host";
const V4 = "example-host-v4";

/** Milliseconds from the first snapshot until the page has a family, and the family. */
async function settle(v6: Answer, v4: Answer): Promise<{ ms: number; family: string | null }> {
  FakeSocket.answers = new Map([
    [V6, v6],
    [V4, v4],
  ]);
  const client = new WallClient("ws://example-coordinator/live");
  const t0 = Date.now();
  let at: number | null = null;
  client.subscribe(() => {
    if (at === null && client.getState().net.family !== null) at = Date.now();
  });
  client.dispatch({ type: "message", text: snapshotText(1, fleet()), at: EPOCH });
  for (let i = 0; i < 200 && at === null; i++) await vi.advanceTimersByTimeAsync(50);
  return { ms: at === null ? Infinity : at - t0, family: client.getState().net.family };
}

describe("the address family probe", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    FakeSocket.opened = [];
    vi.stubGlobal("WebSocket", FakeSocket);
  });
  afterEach(() => {
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  it("streams over IPv4 within a second when IPv6 never answers", async () => {
    const r = await settle("silent", { open: 180 });
    expect(r.family).toBe("v4");
    expect(r.ms).toBeLessThan(1000);
  });

  it("streams over IPv4 at once when IPv6 is refused", async () => {
    const r = await settle({ fail: 2 }, { open: 180 });
    expect(r.family).toBe("v4");
    expect(r.ms).toBeLessThan(500);
  });

  it("prefers IPv6 when it answers within its head start", async () => {
    const r = await settle({ open: 180 }, { open: 20 });
    expect(r.family).toBe("v6");
    expect(r.ms).toBeLessThan(250);
  });

  it("takes IPv6 when it answers first after the head start", async () => {
    const r = await settle({ open: 300 }, { open: 180 });
    expect(r.family).toBe("v6");
    expect(r.ms).toBeLessThan(500);
  });

  it("settles on IPv4 after three probes in which nothing answers", async () => {
    // 3 x FAMILY_PROBE_MS (1.5 s) + 2 x FAMILY_PROBE_RETRY_MS (1 s): nothing can stream meanwhile.
    const r = await settle("silent", "silent");
    expect(r.family).toBe("v4");
    expect(r.ms).toBe(6500);
  });

  it("probes again when every socket of both families is refused, as while renderers start", async () => {
    const r = await settle({ fail: 2 }, { fail: 2 });
    // Every probe refused, three times over: IPv4, after 2 + 2 ms per probe and 1 s between.
    expect(r.family).toBe("v4");
    expect(r.ms).toBe(2012);
    expect(FakeSocket.opened.length).toBe(3 * 6);
  });

  it("streams over IPv6 when IPv4 never answers", async () => {
    const r = await settle({ open: 180 }, "silent");
    expect(r.family).toBe("v6");
    expect(r.ms).toBeLessThan(250);
  });
});
