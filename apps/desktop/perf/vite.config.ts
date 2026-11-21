import { defineConfig } from "vite";

// The AC-9 test page (perf/index.html), built into dist-perf/ so the mocked backend never
// reaches dist/, which the app bundles. Run from apps/desktop: npm run perf:build.
export default defineConfig({
  clearScreen: false,
  build: {
    target: "es2022",
    outDir: "dist-perf",
    emptyOutDir: true,
    rollupOptions: { input: "perf/index.html" },
  },
  preview: { host: "127.0.0.1", port: 4173, strictPort: true },
});
