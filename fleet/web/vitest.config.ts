import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    include: ["core/test/**/*.test.ts", "mock/test/**/*.test.ts"],
    environment: "node",
    testTimeout: 20_000,
    // The mock suite's setup encodes a short clip with ffmpeg before it starts.
    hookTimeout: 60_000,
  },
});
