import { describe, expect, it } from "vitest";

import { shouldRegisterServiceWorker } from "./pwa";

describe("the installed shell", () => {
  it("registers on HTTPS and on a local test host", () => {
    expect(shouldRegisterServiceWorker({ protocol: "https:", hostname: "neha.example.com" })).toBe(
      true,
    );
    expect(shouldRegisterServiceWorker({ protocol: "http:", hostname: "localhost" })).toBe(true);
    expect(shouldRegisterServiceWorker({ protocol: "http:", hostname: "127.0.0.1" })).toBe(true);
  });

  it("leaves the local Space and the desktop shell alone", () => {
    expect(
      shouldRegisterServiceWorker({ protocol: "http:", hostname: "neha-abc123.localhost" }),
    ).toBe(false);
    expect(shouldRegisterServiceWorker({ protocol: "http:", hostname: "192.0.2.1" })).toBe(false);
  });
});
