import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { dayLabel, firstName, timeOfDay } from "./format";

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(new Date(2026, 10, 26, 12, 0, 0));
});

afterEach(() => {
  vi.useRealTimers();
});

describe("firstName", () => {
  it("keeps a single name", () => {
    expect(firstName("Ada")).toBe("Ada");
  });

  it("takes the first word of a full name", () => {
    expect(firstName("Ada Lovelace")).toBe("Ada");
  });

  it("trims surrounding space", () => {
    expect(firstName("  Grace Hopper  ")).toBe("Grace");
  });
});

describe("dayLabel", () => {
  it("names today with its date", () => {
    expect(dayLabel(new Date().toISOString())).toMatch(/^Today · /);
  });

  it("names today in Chinese", () => {
    expect(dayLabel(new Date().toISOString(), "zh-CN")).toMatch(/^今天 · \d+ 月 \d+ 日$/);
  });

  it("names another day in Chinese with its weekday", () => {
    expect(dayLabel(new Date(2026, 10, 24, 12).toISOString(), "zh-CN")).toBe("11 月 24 日 周二");
  });

  it("names another day in English with its weekday", () => {
    expect(dayLabel(new Date(2026, 10, 24, 12).toISOString(), "en-US")).toBe("Tue, Nov 24");
  });
});

describe("timeOfDay", () => {
  it("uses a 24-hour clock", () => {
    expect(timeOfDay(new Date(2026, 10, 26, 9, 44).toISOString())).toBe("09:44");
  });
});
