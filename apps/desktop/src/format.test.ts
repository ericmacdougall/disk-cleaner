import { countOf, formatBytes, formatCount, percentOf } from "./format";

describe("formatBytes", () => {
  it("uses binary units", () => {
    expect(formatBytes(0)).toBe("0 B");
    expect(formatBytes(1023)).toBe("1023 B");
    expect(formatBytes(1024)).toBe("1.0 KiB");
    expect(formatBytes(5 * 1024 ** 3)).toBe("5.0 GiB");
  });
  it("refuses nonsense", () => {
    expect(formatBytes(-1)).toBe("unknown");
    expect(formatBytes(Number.NaN)).toBe("unknown");
  });
});

describe("percentOf", () => {
  it("never divides by zero", () => {
    expect(percentOf(5, 0)).toBe(0);
    expect(percentOf(0, 0)).toBe(0);
  });
  it("is capped at 100", () => {
    expect(percentOf(3, 2)).toBe(100);
    expect(percentOf(1, 4)).toBe(25);
  });
});

describe("formatCount", () => {
  it("groups thousands", () => {
    expect(formatCount(1234567)).toBe("1,234,567");
  });
});

describe("countOf", () => {
  it("agrees with the count", () => {
    expect(countOf(0, "file", "files")).toBe("0 files");
    expect(countOf(1, "file", "files")).toBe("1 file");
    expect(countOf(2, "day", "days")).toBe("2 days");
    expect(countOf(1001, "item", "items")).toBe("1,001 items");
  });
});
