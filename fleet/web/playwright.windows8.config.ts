import { defineConfig, devices } from "@playwright/test";
import base, { mockServer, PORTS } from "./playwright.config.ts";

// Windows mode with eight sandboxes on a 390 px phone, the narrowest captions the page draws
// (two rows of four with the take-over panel open). windows.spec.ts runs four on its own mock.
export default defineConfig({
  ...base,
  testMatch: "windows8.spec.ts",
  testIgnore: undefined,
  projects: [{ name: "phone-390", use: { ...devices["Pixel 7"], viewport: { width: 390, height: 844 } } }],
  webServer: {
    ...mockServer,
    url: `http://localhost:${PORTS.web}/`,
    env: {
      ...mockServer.env,
      FLEET_MOCK_WINDOWS: "8",
      FLEET_MOCK_WINDOWS_CAP: "2",
      FLEET_MOCK_WINDOWS_CLAIM_S: "120",
      FLEET_MOCK_WINDOWS_COOLDOWN_S: "0",
      FLEET_MOCK_WINDOWS_RESTORE_MS: "3000",
    },
  },
});
