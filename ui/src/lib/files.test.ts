import { describe, expect, it } from "vitest";

import { formatBytes, mediaTypeFor } from "./files";

describe("files", () => {
  it("formats a size the way a reader expects", () => {
    expect(formatBytes(0)).toBe("0 B");
    expect(formatBytes(20)).toBe("20 B");
    expect(formatBytes(1536)).toBe("1.5 KB");
    expect(formatBytes(32 * 1024 * 1024)).toBe("32 MB");
    expect(formatBytes(1.5 * 1024 * 1024 * 1024)).toBe("1.5 GB");
  });

  it("keeps the browser's type and names the rest as opaque bytes", () => {
    const typed = new File(["x"], "a.txt", { type: "text/plain" });
    const untyped = new File(["x"], "main.rs");
    expect(mediaTypeFor(typed)).toBe("text/plain");
    expect(mediaTypeFor(untyped)).toBe("application/octet-stream");
  });
});
