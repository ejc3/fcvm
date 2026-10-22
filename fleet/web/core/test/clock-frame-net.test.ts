import { describe, expect, it } from "vitest";
import { clockReceived, initialClock, pingSent, serverNow, shouldPing, PING_TIMEOUT_MS } from "../clock.ts";
import { admitChunk, heightSent, heightToSend, MAX_DECODE_QUEUE, newStream, streamOpened, wantHeight } from "../stream.ts";
import { buildFrame, codecFromSps, parseFrame, parseStreamHeader, splitNals, nalType } from "../frame.ts";
import { frameSocketUrl, reconnectDelayMs, urlHost } from "../net.ts";

describe("clock sync", () => {
  it("estimates the offset from a symmetric round trip", () => {
    let c = pingSent(initialClock(), 1000);
    c = clockReceived(c, 51_040, 1080); // server is 50,000 ahead, 40 ms each way
    expect(c.offsetMs).toBe(50_000);
    expect(serverNow(c, 2000)).toBe(52_000);
  });

  it("keeps the estimate from the sample with the smallest round trip", () => {
    let c = pingSent(initialClock(), 0);
    c = clockReceived(c, 50_010, 20); // rtt 20, offset 50,000
    c = pingSent(c, 100);
    c = clockReceived(c, 50_400, 500); // rtt 400, asymmetric: offset 50,100
    expect(c.offsetMs).toBe(50_000);
    expect(c.rttMs).toBe(20);
  });

  it("ignores a clock message with no ping to pair with", () => {
    const c = clockReceived(initialClock(), 99, 10);
    expect(c.offsetMs).toBeNull();
  });

  it("uses the echo when the coordinator sends one", () => {
    const c = clockReceived(initialClock(), 1_000_050, 100, 0);
    expect(c.offsetMs).toBe(1_000_000);
  });
});

describe("frame messages", () => {
  const au = new Uint8Array([0, 0, 0, 1, 0x09, 0xf0, 0, 0, 0, 1, 0x65, 1, 2, 3]);

  it("round-trips a header", () => {
    const msg = buildFrame({ keyframe: true, width: 320, height: 180, t: 12.375, frame: 4242 }, au);
    const p = parseFrame(msg);
    expect(p.ok).toBe(true);
    if (!p.ok) return;
    expect(p.frame).toMatchObject({ keyframe: true, width: 320, height: 180, t: 12.375, frame: 4242 });
    expect(Array.from(p.frame.data)).toEqual(Array.from(au));
  });

  it("reads the fixed byte layout", () => {
    const msg = buildFrame({ keyframe: false, width: 0x0102, height: 0x0304, t: 1, frame: 7 }, au);
    expect(Array.from(msg.subarray(0, 10))).toEqual([0x46, 0x43, 0x56, 0x46, 1, 0, 0x02, 0x01, 0x04, 0x03]);
    expect(Array.from(msg.subarray(18, 22))).toEqual([7, 0, 0, 0]);
  });

  it.each([
    ["bad magic", (m: Uint8Array) => { m[0] = 0x58; }],
    ["another version", (m: Uint8Array) => { m[4] = 2; }],
    ["zero width", (m: Uint8Array) => { m[6] = 0; m[7] = 0; }],
  ])("refuses %s", (_n, spoil) => {
    const msg = buildFrame({ keyframe: true, width: 2, height: 2, t: 0, frame: 0 }, au);
    spoil(msg);
    expect(parseFrame(msg).ok).toBe(false);
  });

  it("splits Annex-B into NAL units", () => {
    const nals = splitNals(au);
    expect(nals.map(nalType)).toEqual([9, 5]);
  });

  it("names the codec from an SPS", () => {
    expect(codecFromSps(new Uint8Array([0x67, 0x42, 0xc0, 0x1f, 0x00]))).toBe("avc1.42C01F");
  });

  it("reads a stream header and refuses a non-H.264 codec", () => {
    expect(parseStreamHeader('{"codec":"avc1.42E01E","width":256,"height":144}')).toEqual({ codec: "avc1.42E01E", width: 256, height: 144 });
    expect(parseStreamHeader('{"codec":"vp8","width":256,"height":144}')).toBeNull();
  });
});

describe("connections", () => {
  it("backs off from half a second, doubling, to ten", () => {
    expect([0, 1, 2, 3, 4, 5, 20].map(reconnectDelayMs)).toEqual([500, 1000, 2000, 4000, 8000, 10_000, 10_000]);
  });

  it("sets h on a frame URL, replacing an old one", () => {
    expect(frameSocketUrl("wss://slot-3.example.test/frames", 240)).toBe("wss://slot-3.example.test/frames?h=240");
    expect(frameSocketUrl("ws://a/frames?x=1&h=90", 240)).toBe("ws://a/frames?x=1&h=240");
  });

  it("finds the host of an IPv6 URL", () => {
    expect(urlHost("wss://[2001:db8::3]:443/frames")).toBe("[2001:db8::3]");
    expect(urlHost("wss://slot-3.example.test/frames")).toBe("slot-3.example.test");
  });
});

describe("review findings, pinned (clock and streams)", () => {
  it("sends no second ping while one is unanswered, unless it timed out", () => {
    let c = pingSent(initialClock(), 1000);
    expect(shouldPing(c, 1250)).toBe(false);
    c = clockReceived(c, 1150 + 50_000, 1300);
    expect(shouldPing(c, 1300)).toBe(true);
    const lost = pingSent(initialClock(), 1000);
    expect(shouldPing(lost, 1000 + PING_TIMEOUT_MS + 1)).toBe(true);
  });

  it("sends a height asked for while the frame socket was still opening, once it opens", () => {
    let s = streamOpened(newStream(144), false);
    s = wantHeight(s, 720);
    expect(heightToSend(s)).toBeNull();
    s = streamOpened(s, true);
    expect(heightToSend(s)).toBe(720);
    s = heightSent(s, 720);
    expect(heightToSend(s)).toBeNull();
  });

  it("drops chunks while the decoder is backed up, then waits for a keyframe", () => {
    let r = admitChunk({ waitingKey: false }, { keyframe: false, queueSize: 2 });
    expect(r).toEqual({ decode: true, waitingKey: false });
    r = admitChunk(r, { keyframe: false, queueSize: MAX_DECODE_QUEUE + 1 });
    expect(r).toEqual({ decode: false, waitingKey: true });
    r = admitChunk(r, { keyframe: false, queueSize: 0 });
    expect(r).toEqual({ decode: false, waitingKey: true });
    r = admitChunk(r, { keyframe: true, queueSize: 0 });
    expect(r).toEqual({ decode: true, waitingKey: false });
  });
});
