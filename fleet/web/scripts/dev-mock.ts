// Run the page against the mock fleet: the mock coordinator and renderers, then
// Next.js. Ports come from the environment or from .env.ports (gitignored), never
// from a framework default:
//
//   FLEET_WEB_PORT          the page
//   FLEET_MOCK_COORD_PORT   the mock coordinator
//   FLEET_MOCK_RENDER_PORT  the mock renderers
//
//   node scripts/dev-mock.ts           next dev
//   node scripts/dev-mock.ts --prod    next start (run `pnpm build` first)

import { spawn, type ChildProcess } from "node:child_process";
import { existsSync } from "node:fs";
import { createInterface } from "node:readline";

if (existsSync(".env.ports")) process.loadEnvFile(".env.ports");
const prod = process.argv.includes("--prod");

for (const name of ["FLEET_WEB_PORT", "FLEET_MOCK_COORD_PORT", "FLEET_MOCK_RENDER_PORT"]) {
  if (!process.env[name]) {
    console.error(`${name} is not set. Put it in fleet/web/.env.ports (see README.md).`);
    process.exit(2);
  }
}

const children: ChildProcess[] = [];
const stop = (code: number) => {
  for (const c of children) c.kill("SIGTERM");
  process.exit(code);
};
process.on("SIGINT", () => stop(0));
process.on("SIGTERM", () => stop(0));

const mock = spawn(process.execPath, ["mock/main.ts"], { stdio: ["ignore", "pipe", "inherit"], env: process.env });
children.push(mock);
mock.on("exit", (code) => {
  console.error(`mock exited ${code}`);
  stop(1);
});

const ready = await new Promise<{ coord: number; render: number }>((resolve) => {
  const rl = createInterface({ input: mock.stdout! });
  rl.on("line", (line) => {
    try {
      const m = JSON.parse(line) as { ready?: boolean; coord: number; render: number };
      if (m.ready) resolve(m);
    } catch {
      process.stdout.write(`${line}\n`);
    }
  });
});

const host = process.env.FLEET_MOCK_HOST ?? "localhost";
const env = { ...process.env, FLEET_COORD_URL: `ws://${host}:${ready.coord}/live` };
console.log(`mock coordinator ws://${host}:${ready.coord}/live, renderers on ${ready.render}`);
const next = spawn(process.execPath, ["node_modules/next/dist/bin/next", prod ? "start" : "dev", "-p", process.env.FLEET_WEB_PORT!], {
  stdio: "inherit",
  env,
});
children.push(next);
next.on("exit", (code) => stop(code ?? 1));
