import { describe, expect, it, vi } from "vitest";

import { watchViewportHeight } from "./viewport";

interface FakeViewport {
  height: number;
  addEventListener: (type: string, listener: () => void) => void;
  removeEventListener: (type: string, listener: () => void) => void;
}

describe("the viewport height", () => {
  it("follows the visual viewport and removes it on cleanup", () => {
    const listeners = new Map<string, () => void>();
    const viewport: FakeViewport = {
      height: 640,
      addEventListener: (type, listener) => listeners.set(type, listener),
      removeEventListener: vi.fn(),
    };
    Object.defineProperty(window, "visualViewport", {
      configurable: true,
      value: viewport,
    });

    const stop = watchViewportHeight();
    expect(document.documentElement.style.getPropertyValue("--viewport-height")).toBe("640px");

    viewport.height = 360;
    listeners.get("resize")?.();
    expect(document.documentElement.style.getPropertyValue("--viewport-height")).toBe("360px");

    stop();
    expect(document.documentElement.style.getPropertyValue("--viewport-height")).toBe("");
    expect(viewport.removeEventListener).toHaveBeenCalledWith("resize", expect.any(Function));
    expect(viewport.removeEventListener).toHaveBeenCalledWith("scroll", expect.any(Function));

    Reflect.deleteProperty(window, "visualViewport");
  });

  it("falls back to the window height without a visual viewport", () => {
    Object.defineProperty(window, "visualViewport", {
      configurable: true,
      value: undefined,
    });

    const stop = watchViewportHeight();
    expect(document.documentElement.style.getPropertyValue("--viewport-height")).toBe(
      `${window.innerHeight}px`,
    );
    stop();
  });
});
