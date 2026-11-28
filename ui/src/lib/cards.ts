import type { Card, FileSummary } from "../types";
import { MAX_FILE_BYTES } from "./files";

// The MCP Apps protocol version the host answers with.
export const CARD_PROTOCOL_VERSION = "2026-01-26";

// Who answers a Card's initialize request, and the capabilities this host
// really implements: messages into the Chat and opening a safe link. The host
// offers the Card no tools and no resources of its own, so it advertises
// neither `serverTools` nor `serverResources`.
export const CARD_HOST_INFO = { name: "Neha", version: "1" } as const;
export const CARD_HOST_CAPABILITIES = {
  message: { text: {} },
  openLinks: {},
} as const;

// What a Card's frame may load when the Card names no policy of its own: no
// network at all, its own inline scripts and styles, and data: media. A Card
// cannot reach the Space, a Model, or another origin under this policy.
export const DEFAULT_CARD_CSP =
  "default-src 'none'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; media-src 'self' data:; connect-src 'none'; object-src 'none'";

// The largest Card document the client reads. A Card's HTML is one File, and
// the Space keeps no File beyond its own limit; the read is guarded so a Card
// that names anything larger never becomes a page.
export const MAX_CARD_BYTES = MAX_FILE_BYTES;

// How tall a Card's frame starts, and the range a Card may ask it to occupy.
export const INITIAL_CARD_HEIGHT = 320;
export const MIN_CARD_HEIGHT = 120;
export const MAX_CARD_HEIGHT = 800;

// How tall a Card's frame may become, as its own size-changed messages ask.
export function clampCardHeight(height: number): number {
  return Math.min(MAX_CARD_HEIGHT, Math.max(MIN_CARD_HEIGHT, Math.round(height)));
}

// A value as plain JSON: the same data with no proxy, getter, or other object a
// structured clone refuses. The host's own state is a Solid store, whose
// objects are proxies; a real browser's postMessage throws a DataCloneError on
// one, even though a test's spy sees nothing. JSON is the wire's own shape, so
// a Card receives exactly the data it would have parsed. A field whose value is
// undefined is left out, as JSON does.
export function plainJson<T>(value: T): T {
  if (value === undefined) return value;
  return JSON.parse(JSON.stringify(value)) as T;
}

// The theme a Card shows: the Space draws one light theme and fixes it in
// `color-scheme`, so a Card is told light whatever the host OS prefers.
export function cardTheme(): "light" {
  return "light";
}

// The policy a Card's frame runs under: the Card's own policy when it names
// one, else the host's default. The host hands the proxy the Card's raw HTML
// and this policy; the proxy is what prepends the policy meta, so the client
// never parses the document or starts a subresource load before the policy.
export function cardCsp(csp: string | null | undefined): string {
  return csp && csp.trim() !== "" ? csp : DEFAULT_CARD_CSP;
}

// Where one Card's sandbox proxy is loaded. The route is a fixed page; it takes
// the Card's effective policy as one query value so the page it serves can
// derive the frame rule the app's own navigation is held to — a policy that
// names no `frame-src` leaves the proxy blocking every navigation of the app's
// frame. The whole policy travels percent-encoded as one value, so its `;`,
// quotes, and spaces never split the URL.
export function cardSandboxUrl(csp: string): string {
  return `/api/mcp/card/sandbox?csp=${encodeURIComponent(csp)}`;
}

// Reads a Card's HTML from the Space. A File's bytes are the one transfer the
// command surface leaves to same-origin HTTP; the session cookie rides along,
// and no bearer token is ever handed to the frame.
export async function loadCardHtml(file: string, size: number | null): Promise<string> {
  if (size !== null && size > MAX_CARD_BYTES) throw new Error("card-too-large");
  const response = await fetch(`/api/files/${encodeURIComponent(file)}`, {
    credentials: "same-origin",
  });
  if (!response.ok) throw new Error("card-unavailable");
  const header = response.headers.get("content-length");
  const announced = header === null ? null : Number(header);
  if (announced !== null && Number.isFinite(announced) && announced > MAX_CARD_BYTES) {
    throw new Error("card-too-large");
  }
  const html = await response.text();
  // The header may be missing or wrong, so the bytes the client got decide too.
  if (new TextEncoder().encode(html).byteLength > MAX_CARD_BYTES) throw new Error("card-too-large");
  return html;
}

// Whether a value already has the shape of an MCP tool result: a result whose
// content blocks the Card renders as it was sent.
function callResult(value: unknown): Record<string, unknown> | null {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const result = value as Record<string, unknown>;
  return Array.isArray(result.content) ? result : null;
}

// The JSON-RPC params a Card receives as its tool-result notification: the MCP
// tool result shape, with its content blocks and, when the tool answered with
// data, structuredContent. The Space records the MCP server's answer, so a
// result that already is that shape is handed through unchanged; a Card that
// carries only the tool's value is wrapped in it, with a text fallback.
export function cardToolResult(card: Card): Record<string, unknown> {
  const result = card.result ?? null;
  const sent = callResult(result);
  if (sent) {
    const blocks = sent.content as unknown[];
    const hasText = blocks.some((block) => {
      if (!block || typeof block !== "object") return false;
      const typed = block as Record<string, unknown>;
      return typed.type === "text" && typeof typed.text === "string";
    });
    // Every tool result carries a text fallback, so an app that reads only
    // text still gets the answer.
    return hasText
      ? sent
      : {
          ...sent,
          content: [
            { type: "text", text: JSON.stringify(sent.structuredContent ?? null) },
            ...blocks,
          ],
        };
  }
  const text = typeof result === "string" ? result : JSON.stringify(result);
  const structured =
    result !== null && typeof result === "object" && !Array.isArray(result)
      ? (result as Record<string, unknown>)
      : null;
  return structured === null
    ? { content: [{ type: "text", text }] }
    : { content: [{ type: "text", text }], structuredContent: structured };
}

// The user text a Card asks the host to add to the Chat. The card's message may
// sit under `message` or directly in the params; only user text blocks become a
// Message, so a Card cannot put another kind of content in the Chat.
export function cardMessageText(params: Record<string, unknown>): string | null {
  const message =
    params.message && typeof params.message === "object" && !Array.isArray(params.message)
      ? (params.message as Record<string, unknown>)
      : params;
  if (typeof message.role === "string" && message.role !== "user") return null;
  const content = Array.isArray(message.content)
    ? message.content
    : Array.isArray(params.content)
      ? params.content
      : null;
  if (!content) return null;
  const text = content
    .map((block) => {
      if (!block || typeof block !== "object") return "";
      const typed = block as Record<string, unknown>;
      return typed.type === "text" && typeof typed.text === "string" ? typed.text : "";
    })
    .filter((part) => part.trim() !== "")
    .join("\n")
    .trim();
  return text === "" ? null : text;
}

// The URL a Card may open: the web over TLS, and the loopback address a
// developer runs the Space on. A data:, javascript:, file:, or any other
// scheme is refused, as is a URL that carries another account's credentials.
export function cardLink(url: unknown): string | null {
  if (typeof url !== "string") return null;
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return null;
  }
  if (parsed.username !== "" || parsed.password !== "") return null;
  if (parsed.protocol === "https:") return parsed.href;
  if (
    parsed.protocol === "http:" &&
    (parsed.hostname === "127.0.0.1" || parsed.hostname === "localhost")
  ) {
    return parsed.href;
  }
  return null;
}

// The Card a record event or a read names, when it names a usable one.
export function readCard(value: unknown): Card | null {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const record = value as Record<string, unknown>;
  if (typeof record.kind !== "string" || record.kind.trim() === "") return null;
  return {
    kind: record.kind,
    file: typeof record.file === "string" ? record.file : null,
    input: record.input,
    result: record.result,
    csp: typeof record.csp === "string" ? record.csp : null,
  };
}

// A File a Message's Card reads, as the Chat lists it, or null when the list
// does not know it yet.
export function cardFile(
  files: Record<string, FileSummary>,
  id: string | null | undefined,
): FileSummary | null {
  return id ? (files[id] ?? null) : null;
}
