import { describe, expect, it } from "vitest";

import { reachesOnlyLocally } from "./reach";

describe("reachesOnlyLocally", () => {
  it("recognizes the generated Local Space host and loopback addresses", () => {
    // The Local Space's browser origin is a generated *.localhost name.
    expect(reachesOnlyLocally("neha-a1b2c3.localhost")).toBe(true);
    expect(reachesOnlyLocally("localhost")).toBe(true);
    expect(reachesOnlyLocally("127.0.0.1")).toBe(true);
    expect(reachesOnlyLocally("::1")).toBe(true);
    expect(reachesOnlyLocally("[::1]")).toBe(true);
  });

  it("treats a network Space as reachable elsewhere", () => {
    expect(reachesOnlyLocally("neha.example.com")).toBe(false);
    expect(reachesOnlyLocally("192.168.1.20")).toBe(false);
    expect(reachesOnlyLocally("notlocalhost")).toBe(false);
  });
});
