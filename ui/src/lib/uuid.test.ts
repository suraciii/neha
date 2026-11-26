import { describe, expect, it } from "vitest";
import { uuidv7 } from "./uuid";

const uuidv7Pattern = /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

describe("uuidv7", () => {
  it("is a canonical lowercase UUID version 7", () => {
    expect(uuidv7()).toMatch(uuidv7Pattern);
  });

  it("sorts by creation time", async () => {
    const first = uuidv7();
    await new Promise((resolve) => setTimeout(resolve, 2));
    const second = uuidv7();
    expect(first < second).toBe(true);
  });
});
