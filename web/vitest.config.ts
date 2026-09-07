import { defineConfig, mergeConfig } from "vitest/config";
import viteConfig from "./vite.config";

// Unit tests live next to the code as src/**/*.test.ts (node env, pure logic).
// The Playwright e2e suite (e2e/*.spec.ts) is driven by `test:e2e`, not vitest.
export default mergeConfig(viteConfig, defineConfig({
  test: {
    include: ["src/**/*.test.ts"],
    environment: "node",
  },
}));
