import { defineConfig, devices } from "@playwright/test";

// The GPU mirror's replay test (e2e/mirror.spec.ts): a phone's captured command stream replayed
// on WebGL2 in headless Chrome and compared with the phone's own screenshots. No server.
export default defineConfig({
  testDir: "e2e",
  testMatch: "mirror.spec.ts",
  workers: 1,
  timeout: 600_000,
  reporter: [["list"]],
  projects: [{ name: "mirror", use: { ...devices["Desktop Chrome"], viewport: { width: 720, height: 1280 }, deviceScaleFactor: 1 } }],
});
