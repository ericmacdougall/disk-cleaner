import { defineConfig } from "@playwright/test";

// Spec AC-9 (PR gate F9), headless Chromium over the built test page (npm run perf:tree).
// The window matches the app's (1200 x 800, src-tauri/tauri.conf.json).
export default defineConfig({
  testDir: ".",
  testMatch: "*.spec.ts",
  timeout: 300_000,
  workers: 1,
  fullyParallel: false,
  retries: 0,
  reporter: [["list"]],
  outputDir: "../test-results/perf",
  use: {
    browserName: "chromium",
    headless: true,
    viewport: { width: 1200, height: 800 },
    baseURL: "http://127.0.0.1:4173",
  },
  webServer: {
    command: "npx vite preview --config perf/vite.config.ts",
    cwd: "..",
    url: "http://127.0.0.1:4173/perf/index.html",
    reuseExistingServer: false,
    timeout: 60_000,
  },
});
