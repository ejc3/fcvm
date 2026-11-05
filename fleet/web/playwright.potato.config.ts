import { defineConfig, devices } from "@playwright/test";
import base, { mockServer, PORTS } from "./playwright.config.ts";

// Potato mode (`?mode=potato`) against a mock fleet whose host runs eight potato players. The
// wall's suite runs against a mock with none, so its snapshots are unchanged; this one starts its
// own. The pace is set so a ring of 2 drops within a few seconds (it starts at 700 ms of hold) and
// a ring of 8 plays for more than 10 s (it starts at 400 ms), and no wake is slow by chance.
const viewport = base.use!.viewport!;

export default defineConfig({
  ...base,
  testMatch: "potato.spec.ts",
  testIgnore: undefined,
  projects: [{ name: "potato", use: { ...devices["Desktop Chrome"], viewport, deviceScaleFactor: 1 } }],
  webServer: {
    ...mockServer,
    url: `http://localhost:${PORTS.web}/`,
    env: {
      ...mockServer.env,
      FLEET_MOCK_POTATO_PLAYERS: "8",
      FLEET_MOCK_POTATO_BUDGET_MS: "700",
      FLEET_MOCK_POTATO_FLOOR_MS: "400",
      FLEET_MOCK_POTATO_RESTART_MS: "3000",
      FLEET_MOCK_POTATO_SLOW_WAKE_PERCENT: "0",
    },
  },
});
