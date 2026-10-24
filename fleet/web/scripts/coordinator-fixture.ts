// Builds core/test/fixtures/coordinator.json from the coordinator's own test vectors
// (fleet/core/tests/vectors in #1146): each vector's `final_view` is the whole snapshot
// state a browser receives after its events, and its events name the intents the
// coordinator knows (an unknown one is refused with `unknown intent "<name>"`).
//
//   node scripts/coordinator-fixture.ts <path to fleet/core/tests/vectors> <commit>
//
// Regenerate it whenever the coordinator's vectors change; core/test/contract.test.ts
// and mock/test/contract.test.ts read it.
import { readdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const [dir, commit] = process.argv.slice(2);
if (!dir || !commit) {
  console.error("usage: node scripts/coordinator-fixture.ts <fleet/core/tests/vectors> <commit>");
  process.exit(2);
}

type Vector = {
  events?: { type?: string; name?: string }[];
  final_view?: Record<string, unknown> & { actions?: { name?: string; summary?: string }[] };
};

const views: Record<string, unknown> = {};
const sent = new Set<string>();
const unknown = new Set<string>();
for (const file of readdirSync(dir).filter((f) => f.endsWith(".json")).sort()) {
  const v = JSON.parse(readFileSync(join(dir, file), "utf8")) as Vector;
  if (!v.final_view) throw new Error(`${file} has no final_view`);
  views[file.replace(/\.json$/, "")] = v.final_view;
  for (const e of v.events ?? []) if (e.type === "intent" && typeof e.name === "string") sent.add(e.name);
  for (const a of v.final_view.actions ?? []) {
    if (typeof a.name === "string" && typeof a.summary === "string" && a.summary.startsWith("unknown intent")) unknown.add(a.name);
  }
}
if (Object.keys(views).length === 0) throw new Error(`no vectors in ${dir}`);

const out = {
  source: { pr: 1146, commit, dir: "fleet/core/tests/vectors" },
  intents: { known: [...sent].filter((n) => !unknown.has(n)).sort(), unknown: [...unknown].sort() },
  views,
};
writeFileSync("core/test/fixtures/coordinator.json", JSON.stringify(out) + "\n");
console.log(`${Object.keys(views).length} views, intents ${out.intents.known.join(", ")}`);
