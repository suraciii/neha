import { describe, expect, it } from "vitest";

import {
  cardCsp,
  cardLink,
  cardMessageText,
  cardSandboxUrl,
  cardTheme,
  cardToolResult,
  clampCardHeight,
  DEFAULT_CARD_CSP,
  MAX_CARD_HEIGHT,
  MIN_CARD_HEIGHT,
  plainJson,
  readCard,
} from "./cards";

describe("cards", () => {
  it("gives a card the host's safe policy when it names none of its own", () => {
    expect(cardCsp(null)).toBe(DEFAULT_CARD_CSP);
    expect(cardCsp(undefined)).toBe(DEFAULT_CARD_CSP);
    expect(cardCsp("")).toBe(DEFAULT_CARD_CSP);
    expect(cardCsp("   ")).toBe(DEFAULT_CARD_CSP);
    // The safe policy forbids the network, so a Card cannot reach another
    // origin or the Space under it.
    expect(cardCsp(null)).toContain("default-src 'none'");
    expect(cardCsp(null)).toContain("connect-src 'none'");
  });

  it("uses the policy a card names exactly", () => {
    const csp = `default-src 'none'; img-src "https://a.example" data:; report-uri & <x>`;
    expect(cardCsp(csp)).toBe(csp);
    // The client hands the policy to the proxy raw: the proxy escapes it when
    // it prepends the meta, so the client never builds markup of its own.
    expect(cardCsp(csp)).not.toContain("&quot;");
  });

  it("builds the proxy frame's URL with the card's policy as one query value", () => {
    expect(cardSandboxUrl(DEFAULT_CARD_CSP)).toBe(
      `/api/mcp/card/sandbox?csp=${encodeURIComponent(DEFAULT_CARD_CSP)}`,
    );
    // A declared frame-src survives into the query, so the proxy derives the
    // rule the app frame's own navigation is held to from the policy the Card
    // named. The whole policy is one encoded value: its semicolons, quotes, and
    // spaces never split the URL.
    const policy = "default-src 'none'; frame-src https://embed.example.com; img-src data:";
    const url = cardSandboxUrl(policy);
    expect(url.split("?csp=")[1]).toBe(encodeURIComponent(policy));
    expect(new URL(url, "http://space.test").searchParams.get("csp")).toBe(policy);
  });

  it("copies a value to plain JSON, the shape a frame boundary needs", () => {
    // The host's store objects are proxies, which a structured clone refuses.
    const proxied = new Proxy({ answer: 42, nested: { ok: true } }, {});
    expect(() => structuredClone(proxied)).toThrow();
    const plain = plainJson(proxied);
    expect(() => structuredClone(plain)).not.toThrow();
    expect(structuredClone(plain)).toEqual({ answer: 42, nested: { ok: true } });
    // A field whose value is undefined is left out, as JSON does.
    expect(plainJson({ answer: undefined, ok: true })).toEqual({ ok: true });
    expect(plainJson(undefined)).toBeUndefined();
  });

  it("opens only the web over TLS and the loopback Space", () => {
    expect(cardLink("https://example.com/a?b=c")).toBe("https://example.com/a?b=c");
    expect(cardLink("http://localhost:7337/healthz")).toBe("http://localhost:7337/healthz");
    expect(cardLink("http://127.0.0.1:7337/healthz")).toBe("http://127.0.0.1:7337/healthz");
    for (const url of [
      "javascript:alert(1)",
      "data:text/html,hi",
      "file:///etc/passwd",
      "http://example.com/a",
      "ftp://example.com/a",
      "https://user:pass@example.com/a",
      "not a url",
    ]) {
      expect(cardLink(url), String(url)).toBeNull();
    }
    expect(cardLink(null)).toBeNull();
    expect(cardLink(7)).toBeNull();
  });

  it("hands a tool result through and wraps a bare answer", () => {
    // The Space records the MCP call's answer: the Card renders it as it came.
    const sent = {
      content: [{ type: "text", text: "the answer" }],
      structuredContent: { answer: 42 },
    };
    expect(cardToolResult({ kind: "app", result: sent })).toEqual(sent);

    // A result whose blocks carry no text still gets the required fallback.
    const data = { content: [{ type: "image", data: "AA==", mimeType: "image/png" }] };
    expect(cardToolResult({ kind: "app", result: data })).toEqual({
      content: [
        { type: "text", text: "null" },
        { type: "image", data: "AA==", mimeType: "image/png" },
      ],
    });

    // A Card that carries only the tool's value gets the documented shape.
    expect(cardToolResult({ kind: "app", result: { answer: 42 } })).toEqual({
      content: [{ type: "text", text: JSON.stringify({ answer: 42 }) }],
      structuredContent: { answer: 42 },
    });
    expect(cardToolResult({ kind: "app", result: "the answer" })).toEqual({
      content: [{ type: "text", text: "the answer" }],
    });
    expect(cardToolResult({ kind: "app", result: null })).toEqual({
      content: [{ type: "text", text: "null" }],
    });
  });

  it("reads the user text a card asks to put in the Chat", () => {
    expect(
      cardMessageText({
        message: { role: "user", content: [{ type: "text", text: "Use 42." }] },
      }),
    ).toBe("Use 42.");
    expect(cardMessageText({ role: "user", content: [{ type: "text", text: "Direct." }] })).toBe(
      "Direct.",
    );
    expect(
      cardMessageText({
        message: {
          role: "user",
          content: [
            { type: "text", text: "One" },
            { type: "image", data: "AA==", mimeType: "image/png" },
            { type: "text", text: "Two" },
          ],
        },
      }),
    ).toBe("One\nTwo");
    expect(
      cardMessageText({ message: { role: "assistant", content: [{ type: "text", text: "x" }] } }),
    ).toBeNull();
    expect(
      cardMessageText({ message: { role: "user", content: [{ type: "text", text: "  " }] } }),
    ).toBeNull();
    expect(cardMessageText({})).toBeNull();
  });

  it("bounds the frame height the card asks for", () => {
    expect(clampCardHeight(320)).toBe(320);
    expect(clampCardHeight(10)).toBe(MIN_CARD_HEIGHT);
    expect(clampCardHeight(10_000)).toBe(MAX_CARD_HEIGHT);
    expect(clampCardHeight(511.6)).toBe(512);
  });

  it("reads a card out of a record value", () => {
    expect(
      readCard({ kind: "app", file: "f1", input: { q: "hi" }, result: [1], csp: "x" }),
    ).toEqual({
      kind: "app",
      file: "f1",
      input: { q: "hi" },
      result: [1],
      csp: "x",
    });
    expect(readCard({ kind: "future" })).toEqual({
      kind: "future",
      file: null,
      input: undefined,
      result: undefined,
      csp: null,
    });
    expect(readCard(null)).toBeNull();
    expect(readCard([])).toBeNull();
    expect(readCard({})).toBeNull();
    expect(readCard({ kind: "  " })).toBeNull();
  });

  it("always tells a card the light theme the Space draws", () => {
    const original = window.matchMedia;
    Object.defineProperty(window, "matchMedia", {
      configurable: true,
      writable: true,
      value: () => ({ matches: true }),
    });
    try {
      // The page prefers a dark scheme, but the Space fixes a light one in
      // `color-scheme`, so the Card is still told light.
      expect(cardTheme()).toBe("light");
    } finally {
      Object.defineProperty(window, "matchMedia", {
        configurable: true,
        writable: true,
        value: original,
      });
    }
  });
});
