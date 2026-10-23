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
// A smaller screen and frame rate let a small CI runner draw the whole mock fleet.
const [vw, vh] = (process.env.FLEET_E2E_VIEW ?? "1440x900").split("x").map(Number) as [number, number];
const viewport = { width: vw, height: vh };
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
    viewport,
    deviceScaleFactor: 1,
    trace: "retain-on-failure",
  },
  projects: [
    {
      name: "chromium",
      testIgnore: /screenshots\.spec\.ts/,
      use: { ...devices["Desktop Chrome"], viewport, deviceScaleFactor: 1 },
    },
    // WebKit (Playwright's Linux build, not Apple's Safari) runs the tests tagged
    // @engines: the mosaic, the seams and hover.
    {
      name: "webkit",
      grep: /@engines/,
      testIgnore: /screenshots\.spec\.ts/,
      dependencies: ["chromium"],
      use: { ...devices["Desktop Safari"], viewport, deviceScaleFactor: 1 },
    },
    // Last, because it writes words and lights into the shared scene.
    {
      name: "screenshots",
      testMatch: /screenshots\.spec\.ts/,
      dependencies: ["chromium", "webkit"],
      use: { ...devices["Desktop Chrome"], viewport, deviceScaleFactor: 1 },
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
      FLEET_MOCK_FCVM_MS: "3000",
      FLEET_MOCK_EC2_MS: "12000",
      FLEET_MOCK_FCVM_GPUS: "8",
      FLEET_MOCK_EC2_GPUS: "7",
      FLEET_MOCK_GPUS_PER_HOST: "4",
      // Races only when a test starts one; no cooldown or capacity wait unless a test asks.
      FLEET_MOCK_RACE_EVERY_S: "0",
      FLEET_MOCK_COOLDOWN_S: "0",
      FLEET_MOCK_WAIT_PERCENT: "0",
      FLEET_MOCK_BORROW_CAP: "1",
      FLEET_MOCK_CLAIM_S: "8",
      FLEET_MOCK_HOLD_S: "600",
      // The fleet sleeps only when a test puts it to sleep; a wake takes 6 s.
      FLEET_MOCK_SLEEP_AFTER_S: "0",
      FLEET_MOCK_WAKE_MS: "6000",
      FLEET_MOCK_FPS: process.env.FLEET_MOCK_FPS ?? "15",
      FLEET_MOCK_THREADS: process.env.FLEET_MOCK_THREADS ?? "12",
    },
  },
});
