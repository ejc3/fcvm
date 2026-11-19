// The page loads its scripts from its own origin. A script on a host without IPv6 (IVS's
// player.live-video.net has no AAAA record) is one an IPv6-only viewer cannot load, and the
// broadcast it carries would never play for them.

import { createHash } from "node:crypto";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { IVS_PLAYER_URL, IVS_PLAYER_VERSION } from "../broadcast.ts";

const root = fileURLToPath(new URL("../../", import.meta.url));

/**
 * The only other hosts the page's code names. Turnstile's api.js is served only by Cloudflare
 * (a self-hosted copy is not supported) from a host with IPv6; the rest are links, not scripts.
 */
const ALLOWED_HOSTS = new Set(["challenges.cloudflare.com", "github.com", "ejc3.github.io"]);

/** Every source file the page or the Worker is built from. */
function sources(dir: string): string[] {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) return name === "test" || name === "node_modules" ? [] : sources(path);
    return /\.(ts|tsx|js|mjs|json)$/.test(name) ? [path] : [];
  });
}

describe("scripts come from the page's own origin", () => {
  it("names no host outside the allowed few in the page's code", () => {
    const named: string[] = [];
    for (const file of [...sources(join(root, "app")), ...sources(join(root, "core")), join(root, "worker.ts")]) {
      for (const m of readFileSync(file, "utf8").matchAll(/\bhttps?:\/\/([a-z0-9.-]+)/gi)) {
        const host = m[1]!.toLowerCase().replace(/\.$/, "");
        // Placeholders in comments and examples, and the fleet's own hosts.
        if (host.endsWith(".example") || host === "example.com" || host === "localhost" || host === "fcvm.dev" || host.endsWith(".fcvm.dev")) continue;
        if (!ALLOWED_HOSTS.has(host)) named.push(`${relative(root, file)}: ${host}`);
      }
    }
    expect(named).toEqual([]);
  });

  it("loads the IVS player from a path on the page's origin, where the pinned files are", () => {
    expect(IVS_PLAYER_URL).toBe(`/ivs/${IVS_PLAYER_VERSION}/amazon-ivs-player.min.js`);
    expect(statSync(join(root, "public", IVS_PLAYER_URL)).isFile()).toBe(true);
  });

  it("serves exactly the files its checksums name, unchanged, with their license", () => {
    const dir = join(root, "public/ivs", IVS_PLAYER_VERSION);
    const sums = readFileSync(join(dir, "SHA256SUMS"), "utf8")
      .trim()
      .split("\n")
      .map((line) => line.split(/\s+/) as [string, string]);
    const listed = sums.map(([, name]) => name).sort();
    // The player finds its workers and wasm beside its own URL; the license travels with them.
    expect(listed).toEqual(["LICENSE.txt", "amazon-ivs-player.min.js", "amazon-ivs-wasmworker.min.js", "amazon-ivs-wasmworker.min.wasm", "amazon-ivs-worker.min.js"]);
    expect(readdirSync(dir).sort()).toEqual([...listed, "SHA256SUMS"].sort());
    for (const [sum, name] of sums) {
      expect(createHash("sha256").update(readFileSync(join(dir, name))).digest("hex"), name).toBe(sum);
    }
  });
});
