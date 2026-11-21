// Number formatting shared by every view. Binary units, as the core reports allocated bytes.

const UNITS = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];

/** Bytes as a short human string: 0 B, 1023 B, 1.0 KiB, 12.3 GiB. */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "unknown";
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return unit === 0 ? `${value} B` : `${value.toFixed(1)} ${UNITS[unit]}`;
}

/** Part of a whole as a percentage with one decimal; 0 when the whole is 0. */
export function percentOf(part: number, whole: number): number {
  if (!(whole > 0) || !(part > 0)) return 0;
  return Math.min(100, (part / whole) * 100);
}

/** A percentage with one decimal. */
export function formatPercent(p: number): string {
  if (!Number.isFinite(p) || p < 0) return "0.0%";
  return `${Math.min(100, p).toFixed(1)}%`;
}

/** A Unix time as a local date (YYYY-MM-DD), or a dash when unknown. */
export function formatDate(unix: number | null | undefined): string {
  if (unix === null || unix === undefined || !Number.isFinite(unix)) return "-";
  const d = new Date(unix * 1000);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

/** Thousands separators for counts. */
export function formatCount(n: number): string {
  return Math.round(n).toLocaleString("en-US");
}

/** A count with the noun that agrees with it: "1 file", "2 files". */
export function countOf(n: number, one: string, many: string): string {
  return `${formatCount(n)} ${Math.round(n) === 1 ? one : many}`;
}
