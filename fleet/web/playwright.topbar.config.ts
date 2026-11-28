import { defineConfig, devices } from "@playwright/test";
import base, { mockServer, PORTS } from "./playwright.config.ts";

// The top bar of every mode on phone and desktop screens, against a mock fleet that runs every
// mode's content, so each page draws the controls it has when the fleet is up.
export default defineConfig({
  ...base,
  testMatch: "topbar.spec.ts",
  testIgnore: undefined,
  projects: [{ name: "topbar", use: { ...devices["Desktop Chrome"], deviceScaleFactor: 1 } }],
  webServer: {
    ...mockServer,
    url: `http://localhost:${PORTS.web}/`,
    env: {
      ...mockServer.env,
      FLEET_MOCK_PHONES: "4",
      FLEET_MOCK_PHONE_ZOOM_GAP_MS: "2000",
      FLEET_MOCK_WINDOWS: "4",
      FLEET_MOCK_POTATO_PLAYERS: "8",
      FLEET_MOCK_MINECRAFT: "4",
      FLEET_MOCK_MINECRAFT_PORT: String(PORTS.minecraft),
      FLEET_MOCK_MINECRAFT_JOIN_ZONE: "mc.example.test",
    },
  },
});
