import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { create } from "fontkit";
import { describe, expect, it } from "vitest";

import { catalogs } from "./i18n";
import { dayLabel } from "./lib/format";

describe("bundled CJK font", () => {
  it("covers every non-ASCII character in the Chinese catalog", () => {
    const font = create(
      readFileSync(resolve(process.cwd(), "public/fonts/NotoSansSC-subset.woff2")),
    );
    const dateSamples = [
      new Date(),
      ...Array.from({ length: 7 }, (_, index) => new Date(2026, 10, 22 + index, 12)),
    ];
    const generated = dateSamples.map((date) => dayLabel(date.toISOString(), "zh-CN"));
    const characters = new Set(
      [...Object.values(catalogs["zh-CN"]), ...generated]
        .join("")
        .split("")
        .filter((char) => char.charCodeAt(0) > 0x7f),
    );
    if (!("hasGlyphForCodePoint" in font)) {
      throw new Error("the bundled font is a collection, not a single font");
    }
    const missing = [...characters].filter(
      (char) => !font.hasGlyphForCodePoint(char.codePointAt(0) ?? 0),
    );
    expect(missing).toEqual([]);
  });
});
