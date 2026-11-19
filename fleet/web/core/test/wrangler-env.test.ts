// The production Worker's configuration (wrangler.jsonc `env.production`): its own name and
// coordinator, no public workers.dev or preview address, and nothing that depends on Access.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { liveProxy, pageDirectLiveUrl, pageLiveUrl, type ProxyEnv } from "../live-proxy.ts";

/** wrangler.jsonc as JSON: its comments dropped, outside strings only (URLs hold "//"). */
function readJsonc(path: string): Record<string, unknown> {
  const text = readFileSync(path, "utf8");
  let out = "";
  let inString = false;
  for (let i = 0; i < text.length; i++) {
    const c = text[i]!;
    if (inString) {
      out += c;
      if (c === "\\") out += text[++i] ?? "";
      else if (c === '"') inString = false;
    } else if (c === '"') {
      inString = true;
      out += c;
    } else if (c === "/" && text[i + 1] === "/") {
      while (i < text.length && text[i] !== "\n") i++;
      out += "\n";
    } else out += c;
  }
  return JSON.parse(out) as Record<string, unknown>;
}

const config = readJsonc(fileURLToPath(new URL("../../wrangler.jsonc", import.meta.url)));
const prod = (config["env"] as Record<string, Record<string, unknown>>)["production"]!;
const vars = prod["vars"] as Record<string, string>;

describe("the production Worker", () => {
  it("is its own Worker, beside staging's, with no workers.dev or preview address", () => {
    expect(config["name"]).toBe("fcvm-fleet-stage");
    expect(prod["name"]).toBe("fcvm-fleet");
    expect(prod["workers_dev"]).toBe(false);
    expect(prod["preview_urls"]).toBe(false);
  });

  it("names the prod coordinator, the prod tiles' subzone, and a placeholder Turnstile site key", () => {
    expect(vars).toEqual({
      FLEET_COORD_URL: "wss://fleet-api.fcvm.dev/live",
      FLEET_TILES_SUBZONE: "tiles.fcvm.dev",
      FLEET_TURNSTILE_SITE_KEY: "PLACEHOLDER-set-by-ejc3-aws-A2",
    });
  });

  it("connects the page to the coordinator itself: no Access token, no forwarding, no Access-only host", () => {
    const env: ProxyEnv = { ...vars };
    expect(liveProxy(env)).toBeNull();
    expect(pageLiveUrl(env, "fcvm.dev", "https")).toBe("wss://fleet-api.fcvm.dev/live");
    expect(pageDirectLiveUrl(env)).toBeNull();
  });
});
