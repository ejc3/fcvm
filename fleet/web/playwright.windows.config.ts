import { defineConfig, devices } from "@playwright/test";
import base, { mockServer, PORTS } from "./playwright.config.ts";

// Windows mode (`?mode=windows`) against a mock fleet that runs four Windows sandboxes, on a
// desktop screen and on a phone. The wall's suite runs against a mock with none, so its
// snapshots are unchanged; this one starts its own.
const viewport = base.use!.viewport!;

export default defineConfig({
  ...base,
  testMatch: "windows.spec.ts",
  testIgnore: undefined,
  projects: [
    { name: "desktop", use: { ...devices["Desktop Chrome"], viewport, deviceScaleFactor: 1 } },
    { name: "phone", use: { ...devices["Pixel 7"] } },
  ],
  webServer: {
    ...mockServer,
    url: `http://localhost:${PORTS.web}/`,
    env: {
      ...mockServer.env,
      FLEET_MOCK_WINDOWS: "4",
      FLEET_MOCK_WINDOWS_CAP: "2",
      FLEET_MOCK_WINDOWS_CLAIM_S: "120",
      // The suite plays several viewers from one machine and borrows again at once.
      FLEET_MOCK_WINDOWS_COOLDOWN_S: "0",
      FLEET_MOCK_WINDOWS_RESTORE_MS: "3000",
    },
  },
});
