import { existsSync } from "node:fs";
import { defineConfig, devices } from "@playwright/test";

// Ports: FLEET_TEST_BASE_PORT (from the environment or .env.ports) and the two
// after it. No framework default port is ever used.
if (existsSync(".env.ports")) process.loadEnvFile(".env.ports");
const base = Number(process.env.FLEET_TEST_BASE_PORT);
if (!Number.isInteger(base) || base <= 0) {
  throw new Error("FLEET_TEST_BASE_PORT is not set. Put it in fleet/web/.env.ports (see README.md).");
}
export const PORTS = { web: base, coord: base + 1, render: base + 2 };
process.env.FLEET_E2E_COORD_PORT = String(PORTS.coord);

export default defineConfig({
  testDir: "e2e",
  testMatch: "*.spec.ts",
  fullyParallel: false,
  workers: 1,
  timeout: 90_000,
  expect: { timeout: 15_000 },
  reporter: [["list"]],
  use: {
    baseURL: `http://localhost:${PORTS.web}`,
    viewport: { width: 1440, height: 900 },
    deviceScaleFactor: 1,
    trace: "retain-on-failure",
  },
  projects: [
    {
      name: "chromium",
      testIgnore: /screenshots\.spec\.ts/,
      use: { ...devices["Desktop Chrome"], viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1 },
    },
    // WebKit (Playwright's Linux build, not Apple's Safari) runs the tests tagged
    // @engines: the mosaic, the seams and hover.
    {
      name: "webkit",
      grep: /@engines/,
      testIgnore: /screenshots\.spec\.ts/,
      dependencies: ["chromium"],
      use: { ...devices["Desktop Safari"], viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1 },
    },
    // Last, because it writes words and lights into the shared scene.
    {
      name: "screenshots",
      testMatch: /screenshots\.spec\.ts/,
      dependencies: ["chromium", "webkit"],
      use: { ...devices["Desktop Chrome"], viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1 },
    },
  ],
  webServer: {
    command: "node scripts/dev-mock.ts --prod",
    url: `http://localhost:${PORTS.web}/`,
    reuseExistingServer: false,
    timeout: 60_000,
    env: {
      FLEET_WEB_PORT: String(PORTS.web),
      FLEET_MOCK_COORD_PORT: String(PORTS.coord),
      FLEET_MOCK_RENDER_PORT: String(PORTS.render),
      FLEET_MOCK_FCVM_MS: "2500",
      FLEET_MOCK_EC2_MS: "12000",
      FLEET_MOCK_TILES: "8",
      FLEET_MOCK_EC2_SHORT: "1",
    },
  },
});
