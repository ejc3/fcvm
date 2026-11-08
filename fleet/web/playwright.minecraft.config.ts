import { defineConfig, devices } from "@playwright/test";
import base, { mockServer, PORTS } from "./playwright.config.ts";

// Minecraft mode (`?mode=minecraft`) against a mock fleet that runs four Minecraft servers, on a
// desktop screen and on a phone. The servers' view sockets listen on the port after the mock
// renderers'. The wall's suite runs against a mock with none, so its snapshots are unchanged.
const viewport = base.use!.viewport!;

export default defineConfig({
  ...base,
  testMatch: "minecraft.spec.ts",
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
      FLEET_MOCK_MINECRAFT: "4",
      FLEET_MOCK_MINECRAFT_PORT: String(PORTS.minecraft),
      FLEET_MOCK_MINECRAFT_CAP: "2",
      FLEET_MOCK_MINECRAFT_CLAIM_S: "120",
      // The suite plays several viewers from one machine and borrows again at once.
      FLEET_MOCK_MINECRAFT_COOLDOWN_S: "0",
      FLEET_MOCK_MINECRAFT_RESTORE_MS: "2000",
      FLEET_MOCK_MINECRAFT_SNAPSHOT_MS: "600",
      FLEET_MOCK_MINECRAFT_JOIN_ZONE: "mc.example.test",
    },
  },
});
