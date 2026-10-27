// player.html's measurement loop against a mocked IVS player and video element:
//   node fleet/tools/ivsmeasure/test_player.mjs
// Each cycle must leave no frame callback registered on the shared <video>, or later
// cycles run every earlier cycle's callback on every frame.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const html = readFileSync(new URL("./player.html", import.meta.url), "utf8");
const script = html.split("<script>")[1].split("</script>")[0];

const realSetTimeout = setTimeout;
globalThis.setTimeout = (f, ms) => realSetTimeout(f, (ms ?? 0) / 100);
const video = {
  next: 1,
  cbs: new Map(),
  requestVideoFrameCallback(f) {
    const id = this.next++;
    this.cbs.set(id, f);
    return id;
  },
  cancelVideoFrameCallback(id) {
    this.cbs.delete(id);
  },
};
// Present a frame every 5 ms: each registered callback runs once, as rVFC's do.
let mediaTime = 0;
const presenter = setInterval(() => {
  const due = [...video.cbs.entries()];
  video.cbs.clear();
  mediaTime += 1 / 30;
  for (const [, f] of due) f(0, { mediaTime, expectedDisplayTime: 0 });
}, 5);
const player = {
  attachHTMLVideoElement() {},
  setMuted() {},
  addEventListener() {},
  load() {},
  play() {},
  getLiveLatency: () => 1.5,
  getQuality: () => null,
  delete() {},
};
globalThis.IVSPlayer = {
  create: () => player,
  PlayerState: { PLAYING: "playing" },
  PlayerEventType: { ERROR: "error" },
  MetadataEventType: { SEI: "sei" },
};
globalThis.document = { getElementById: () => video };
globalThis.location = { search: "?url=x&cycles=3&secs=1" };
const reports = [];
globalThis.fetch = async (_url, init) => {
  reports.push(JSON.parse(init.body));
};

new Function(script)();
const deadline = Date.now() + 20_000;
while (!reports.some((r) => r.done) && Date.now() < deadline) await new Promise((r) => realSetTimeout(r, 10));
clearInterval(presenter);
assert.ok(reports.some((r) => r.done), "the run finished");
assert.equal(reports.filter((r) => "cycle" in r).length, 3);
assert.equal(video.cbs.size, 0, `${video.cbs.size} frame callbacks still registered after the last cycle`);
console.log("ok");
