import { existsSync, readFileSync } from "node:fs";
import { resolve } from "node:path";

import { describe, expect, it } from "vitest";

// The package runs its checks from `ui/`.
const ui = process.cwd();

function uiFile(...parts: string[]): string {
  return resolve(ui, ...parts);
}

describe("the installed client", () => {
  it("declares the shell and every icon it installs", () => {
    const manifest = JSON.parse(readFileSync(uiFile("public", "manifest.webmanifest"), "utf8")) as {
      name: string;
      short_name: string;
      start_url: string;
      scope: string;
      display: string;
      icons: { src: string; sizes: string; type: string; purpose: string }[];
    };

    expect(manifest.name).toBe("Neha");
    expect(manifest.short_name).toBe("Neha");
    expect(manifest.start_url).toBe("/");
    expect(manifest.scope).toBe("/");
    expect(manifest.display).toBe("standalone");
    expect(manifest.icons.map((icon) => icon.sizes)).toEqual(
      expect.arrayContaining(["192x192", "512x512"]),
    );
    expect(manifest.icons.some((icon) => icon.purpose === "maskable")).toBe(true);
    for (const icon of manifest.icons) {
      expect(icon.type).toBe("image/png");
      const iconPath = icon.src.replace(/^\//, "");
      expect(existsSync(uiFile("public", iconPath)), icon.src).toBe(true);
    }
    expect(existsSync(uiFile("public", "icons", "apple-touch-icon.png"))).toBe(true);
  });

  it("lets a phone size the page to the screen and finds the manifest", () => {
    const html = readFileSync(uiFile("index.html"), "utf8");
    expect(html).toContain("viewport-fit=cover");
    expect(html).toContain('rel="manifest"');
    expect(html).toContain('rel="apple-touch-icon"');
  });
});
