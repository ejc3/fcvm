import { builtinModules } from "node:module";
import js from "@eslint/js";
import { defineConfig, globalIgnores } from "eslint/config";
import tseslint from "typescript-eslint";

// core/ runs in the page, in the frame worker, in the mock and in Node tests, so it
// may use nothing any one of them lacks: no platform, no I/O, no clock, no randomness.
const NEUTRAL = "core/ is platform-neutral.";
const CLOCK = "Pass time in; core/ has no clock.";
const PORTABLE_CORE = {
  files: ["core/**/*.ts"],
  ignores: ["core/test/**"],
  rules: {
    "no-restricted-imports": ["error", { patterns: [
      { group: ["node:*", ...builtinModules, ...builtinModules.map((m) => `${m}/*`)], message: NEUTRAL },
      { group: ["react", "react-dom", "next", "next/*", "ws"], message: NEUTRAL }] }],
    "no-restricted-properties": ["error",
      { object: "Math", property: "random", message: "Randomness comes in as an argument." },
      { object: "Date", property: "now", message: CLOCK },
      { object: "performance", property: "now", message: CLOCK },
      { object: "globalThis", message: NEUTRAL }],
    "no-restricted-syntax": ["error",
      { selector: "NewExpression[callee.name='Date']", message: CLOCK },
      { selector: "CallExpression[callee.name='Date']", message: CLOCK }],
    "no-restricted-globals": ["error",
      ...["process", "window", "self", "document", "crypto", "performance", "navigator", "localStorage",
        "indexedDB", "setTimeout", "clearTimeout", "setInterval", "clearInterval", "requestAnimationFrame",
        "WebSocket", "Worker", "VideoDecoder", "URL"].map((name) => ({ name, message: NEUTRAL })),
      { name: "fetch", message: "core/ does no I/O. The caller fetches." }],
  },
};

export default defineConfig([
  js.configs.recommended,
  ...tseslint.configs.strict,
  PORTABLE_CORE,
  // noUncheckedIndexedAccess makes every index read optional; where it is known to be in range, `!` says so.
  { rules: { "@typescript-eslint/no-non-null-assertion": "off" } },
  { files: ["**/*.mjs", "mock/**/*.ts", "scripts/**/*.ts"], languageOptions: { globals: { process: "readonly", console: "readonly" } } },
  globalIgnores(["node_modules/**", ".next/**", ".open-next/**", "test-results/**", "playwright-report/**", "next-env.d.ts"]),
]);
