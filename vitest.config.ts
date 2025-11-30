/// <reference types="vitest" />
import { defineConfig } from "vitest/config";
import path from "path";

export default defineConfig({
  test: {
    include: ["tests/**/*.test.{js,mjs,cjs,ts,mts,cts,jsx,tsx}"],
    // Exclude Playwright E2E tests (*.spec.ts files)
    exclude: ["tests/**/*.spec.{js,mjs,cjs,ts,mts,cts,jsx,tsx}", "node_modules/**"],
    environment: "node",
    globals: true,
  },
  resolve: {
    alias: {
      $lib: path.resolve("./src/lib"),
    },
  },
});
