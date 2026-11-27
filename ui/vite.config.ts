import { fileURLToPath } from "node:url";
import solid from "@solidjs/vite-plugin";
import { defineConfig } from "vitest/config";

// `neha serve` prints the browser hostname; set NEHA_SPACE to that URL for
// dev. The proxy preserves the dev origin's Host so the Space sees the same
// per-Space hostname and accepts the cookie.
const configured = process.env.NEHA_SPACE;
const browser = configured ? new URL(configured) : null;
const backend = browser ? `http://127.0.0.1:${browser.port || "7337"}` : "http://127.0.0.1:7337";

export default defineConfig({
  plugins: [solid()],
  server: {
    host: browser?.hostname,
    port: 5173,
    proxy: {
      "/api": { target: backend, changeOrigin: false, ws: true },
      "/healthz": { target: backend, changeOrigin: false },
    },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    rollupOptions: {
      input: {
        app: fileURLToPath(new URL("./index.html", import.meta.url)),
        desktop: fileURLToPath(new URL("./desktop.html", import.meta.url)),
      },
    },
  },
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.{ts,tsx}"],
  },
});
