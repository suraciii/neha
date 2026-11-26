import { describe, expect, it } from "vitest";
import { dayLabel, firstName, timeOfDay } from "./format";

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
});

describe("timeOfDay", () => {
  it("uses a 24-hour clock", () => {
    expect(timeOfDay(new Date(2026, 10, 26, 9, 44).toISOString())).toBe("09:44");
  });
});
