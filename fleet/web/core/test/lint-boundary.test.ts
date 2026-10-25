import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { expect, it } from "vitest";

const root = fileURLToPath(new URL("../../", import.meta.url));
const cli = join(root, "node_modules/oxlint/bin/oxlint");

it("keeps the core platform boundary while allowing host APIs in tests", () => {
  const directory = mkdtempSync(join(root, "core/lint-probe-"));
  const testDirectory = mkdtempSync(join(root, "core/test/lint-probe-"));
  const lint = (directory: string, source: string) => {
    const path = join(directory, "probe.ts");
    writeFileSync(path, source);
    return spawnSync(process.execPath, [cli, "--threads=1", path], { cwd: root, encoding: "utf8" });
  };
  try {
    for (const source of [
      'import { readFile } from "node:fs"; export { readFile };',
      'import { readFile } from "fs/promises"; export { readFile };',
      'import { useState } from "react"; export { useState };',
      'Math.random();',
      'Date.now();',
      'new Date();',
      'Date();',
      'globalThis.Math.random();',
      'export const value = window;',
      'fetch("https://example.com");',
    ]) {
      const result = lint(directory, source);
      expect(result.status, `${source}\n${result.stdout}\n${result.stderr}`).toBe(1);
    }
    const portable = lint(directory, 'export const identity = (value: number) => value;');
    expect(portable.status, portable.stdout + portable.stderr).toBe(0);
    const host = lint(testDirectory, 'import { readFile } from "node:fs"; export const value = [readFile, Date.now()];');
    expect(host.status, host.stdout + host.stderr).toBe(0);
  } finally {
    rmSync(directory, { recursive: true, force: true });
    rmSync(testDirectory, { recursive: true, force: true });
  }
});
