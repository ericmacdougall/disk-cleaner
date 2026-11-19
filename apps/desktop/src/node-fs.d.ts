// Types for the one Node API the unit tests use (contrast.test.ts reads styles.css); the app
// itself runs in the webview and has no Node types.
declare module "node:fs" {
  export function readFileSync(path: URL, encoding: "utf8"): string;
}
