// Every file the wall page asks for under /media, served by a deployed origin or a local Worker.
// The list comes from core/reel.ts (the page's own source list), plus the metadata the page
// fetches (wall-reel.json), so a file the page starts asking for is checked without an edit here.
//
//   node scripts/media-check.ts <origin> [-4|-6]
//
// With CF_ACCESS_CLIENT_ID and CF_ACCESS_CLIENT_SECRET set, the Access service-token headers go
// to <origin> only, and redirects are not followed, so the headers never reach another host.
// Exit 1 when any file is not a 200 with the expected content type and a body, or when the
// metadata is not the shape the page reads.

import { reelView, parseReelMeta } from "../core/reel.ts";

const origin = process.argv[2];
if (!origin || !/^https?:\/\//.test(origin)) {
  console.error("usage: node scripts/media-check.ts <origin>");
  process.exit(2);
}
const base = origin.replace(/\/$/, "");

const view = reelView({ base: "/media", meta: null, power: "asleep", reelOn: true, allLive: false, reducedMotion: false, saveData: false });
const wanted: { path: string; type: string }[] = [
  ...view.sources.map((s) => ({ path: s.src, type: s.type })),
  { path: view.track.src, type: "text/vtt" },
  { path: "/media/wall-reel.json", type: "application/json" },
];

const id = process.env["CF_ACCESS_CLIENT_ID"];
const secret = process.env["CF_ACCESS_CLIENT_SECRET"];
const headers: Record<string, string> = id && secret ? { "CF-Access-Client-Id": id, "CF-Access-Client-Secret": secret } : {};

let failed = 0;
for (const w of wanted) {
  let line: string;
  try {
    const r = await fetch(`${base}${w.path}`, { headers, redirect: "manual", signal: AbortSignal.timeout(60_000) });
    const body = Buffer.from(await r.arrayBuffer());
    const type = (r.headers.get("content-type") ?? "").split(";")[0]!.trim().toLowerCase();
    const problems: string[] = [];
    if (r.status !== 200) problems.push(`status ${r.status}`);
    if (type !== w.type) problems.push(`content-type ${type || "none"}, want ${w.type}`);
    if (body.length === 0) problems.push("empty body");
    if (r.status === 200 && w.path.endsWith(".json") && parseReelMeta(safeJson(body)) === null) problems.push("not the metadata shape the page reads");
    line = `${w.path}: ${r.status} ${type} ${body.length} bytes${problems.length ? ` FAIL (${problems.join("; ")})` : ""}`;
    if (problems.length) failed++;
  } catch (e) {
    line = `${w.path}: FAIL (${String(e)})`;
    failed++;
  }
  console.log(line);
}
if (failed) {
  console.error(`media-check: ${failed} of ${wanted.length} files the page asks for are not served by ${base}`);
  process.exit(1);
}
console.log(`media-check: all ${wanted.length} files served by ${base}`);

function safeJson(b: Buffer): unknown {
  try {
    return JSON.parse(b.toString("utf8"));
  } catch {
    return undefined;
  }
}
