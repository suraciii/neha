import { describe, expect, it } from "vitest";

import type { PushEnvironment } from "./push";
import { isIos, isMuted, pushCapability, urlBase64ToUint8Array } from "./push";

function environment(overrides: Partial<PushEnvironment> = {}): PushEnvironment {
  return {
    userAgent: "Mozilla/5.0 (Linux; Android 14)",
    platform: "Linux armv8l",
    maxTouchPoints: 5,
    standalone: false,
    registration: true,
    notification: true,
    pushManager: true,
    serviceWorker: true,
    ...overrides,
  };
}

const iphone = {
  userAgent: "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X)",
  platform: "iPhone",
  maxTouchPoints: 5,
};

describe("push capability", () => {
  it("asks an iPhone to install before checking for push APIs", () => {
    expect(
      pushCapability(
        environment({
          ...iphone,
          registration: false,
          notification: false,
          pushManager: false,
          serviceWorker: false,
        }),
      ),
    ).toBe("ios-install");
    expect(pushCapability(environment({ ...iphone, standalone: true }))).toBe("ready");
  });

  it("recognizes an iPad that reports a Mac platform", () => {
    expect(isIos({ userAgent: "Mozilla/5.0 (Macintosh)", platform: "MacIntel", maxTouchPoints: 5 })).toBe(
      true,
    );
    expect(isIos({ userAgent: "Mozilla/5.0 (Macintosh)", platform: "MacIntel", maxTouchPoints: 0 })).toBe(
      false,
    );
  });

  it("is unsupported without a worker or push APIs", () => {
    expect(pushCapability(environment({ registration: false }))).toBe("unsupported");
    expect(pushCapability(environment({ notification: false }))).toBe("unsupported");
    expect(pushCapability(environment({ pushManager: false }))).toBe("unsupported");
    expect(pushCapability(environment({ ...iphone, standalone: true, notification: false }))).toBe(
      "unsupported",
    );
  });

  it("decodes the VAPID key and reads a mute", () => {
    const bytes = urlBase64ToUint8Array("AQID");
    expect([...bytes]).toEqual([1, 2, 3]);
    expect(isMuted(null)).toBe(false);
    expect(isMuted(Math.floor(Date.now() / 1000) + 60)).toBe(true);
    expect(isMuted(Math.floor(Date.now() / 1000) - 60)).toBe(false);
  });
});
