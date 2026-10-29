import { defineConfig, devices } from "@playwright/test";
import base, { mockServer, PORTS } from "./playwright.config.ts";

// Phones mode (`?mode=phones`) against a mock fleet that runs four phones. The wall's suite
// runs against a mock with none, so its snapshots are unchanged; this one starts its own.
const viewport = base.use!.viewport!;

export default defineConfig({
  ...base,
  testMatch: "phones.spec.ts",
  testIgnore: undefined,
  projects: [{ name: "phones", use: { ...devices["Desktop Chrome"], viewport, deviceScaleFactor: 1 } }],
  webServer: {
    ...mockServer,
    url: `http://localhost:${PORTS.web}/`,
    env: {
      ...mockServer.env,
      FLEET_MOCK_PHONES: "4",
      FLEET_MOCK_PHONE_CAP: "2",
      FLEET_MOCK_PHONE_CLAIM_S: "120",
      // The suite plays several viewers from one machine and borrows again at once.
      FLEET_MOCK_PHONE_COOLDOWN_S: "0",
      FLEET_MOCK_PHONE_RESTORE_MS: "2000",
      FLEET_MOCK_PHONE_ZOOM_GAP_MS: "2000",
    },
  },
});
