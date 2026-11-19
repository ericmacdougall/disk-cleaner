// Spec AC-7: text and control colours meet WCAG 2.2 AA in light and dark, from the tokens in
// styles.css (contrast ratio formula of WCAG 2.2).
import { describe, expect, it } from "vitest";
import css from "./styles.css?raw";

function tokens(block: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const m of block.matchAll(/--([a-z-]+):\s*(#[0-9a-fA-F]{6})/g)) out[m[1]!] = m[2]!;
  return out;
}

function luminance(hex: string): number {
  const c = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255);
  const l = c.map((v) => (v <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4));
  return 0.2126 * l[0]! + 0.7152 * l[1]! + 0.0722 * l[2]!;
}

export function contrast(a: string, b: string): number {
  const [x, y] = [luminance(a), luminance(b)].sort((p, q) => q - p);
  return (x! + 0.05) / (y! + 0.05);
}

const light = tokens(css.slice(css.indexOf(":root {"), css.indexOf("}")));
const darkStart = css.indexOf("@media (prefers-color-scheme: dark)");
const dark = { ...light, ...tokens(css.slice(darkStart, css.indexOf("}", css.indexOf(":root {", darkStart)))) };

// [foreground, background, minimum]: 4.5 for text (link buttons use the accent as text), 3 for
// control borders, tile edges and the focus ring (WCAG 1.4.11).
const PAIRS: [string, string, number][] = [
  ["text", "bg", 4.5],
  ["text", "surface", 4.5],
  ["muted", "bg", 4.5],
  ["muted", "surface", 4.5],
  ["accent", "bg", 4.5],
  ["accent", "surface", 4.5],
  ["on-accent", "accent", 4.5],
  ["text", "selected", 4.5],
  ["muted", "selected", 4.5],
  ["text", "tile-dir", 4.5],
  ["text", "tile-file", 4.5],
  ["muted", "tile-dir", 4.5],
  ["muted", "tile-file", 4.5],
  ["border", "bg", 3],
  ["border", "surface", 3],
  ["border", "tile-dir", 3],
  ["border", "tile-file", 3],
  ["accent", "selected", 3],
  ["accent", "tile-dir", 3],
  ["accent", "tile-file", 3],
];

describe("colour contrast (WCAG 2.2 AA)", () => {
  for (const [name, t] of [
    ["light", light],
    ["dark", dark],
  ] as const) {
    for (const [fg, bg, min] of PAIRS) {
      it(`${name}: ${fg} on ${bg} is at least ${min}:1`, () => {
        expect(t[fg], fg).toMatch(/^#/);
        expect(t[bg], bg).toMatch(/^#/);
        expect(contrast(t[fg]!, t[bg]!)).toBeGreaterThanOrEqual(min);
      });
    }
  }

  it("the formula matches known values", () => {
    expect(contrast("#000000", "#ffffff")).toBeCloseTo(21, 5);
    expect(contrast("#777777", "#ffffff")).toBeCloseTo(4.48, 2);
  });
});
