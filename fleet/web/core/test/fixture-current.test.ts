import { readdirSync, readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

// core/test/fixtures/coordinator.json holds the coordinator's final view of each blessed vector
// (fleet/core/tests/vectors). A vector changed without regenerating its view here fails this test,
// so the page and the mock are never checked against a stale copy.
describe("the coordinator fixture is current", () => {
  const dir = new URL("../../../core/tests/vectors/", import.meta.url);
  const fixture = JSON.parse(readFileSync(new URL("./fixtures/coordinator.json", import.meta.url), "utf8")) as { views: Record<string, unknown> };
  const names = readdirSync(dir).filter((f) => f.endsWith(".json")).map((f) => f.slice(0, -5));

  it("reads the vectors", () => {
    expect(names.length).toBeGreaterThan(30);
  });

  for (const name of Object.keys(fixture.views)) {
    it(`${name}: the view is the vector's final view`, () => {
      expect(names, `${name} has no vector`).toContain(name);
      const vector = JSON.parse(readFileSync(new URL(`${name}.json`, dir), "utf8")) as { final_view: unknown };
      expect(fixture.views[name]).toEqual(vector.final_view);
    });
  }
});
