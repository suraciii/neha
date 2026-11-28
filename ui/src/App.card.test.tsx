import { fireEvent, waitFor } from "@solidjs/testing-library";
import { flush } from "solid-js";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { liveRetained } from "./App";
import { setLocale, t } from "./i18n";
import { DEFAULT_CARD_CSP, MAX_CARD_BYTES } from "./lib/cards";
import { appCard, appFile, appHtml, serveFiles, space } from "./test/fakeSpace";
import { drain, element, openChat, record, useView, type View } from "./test/view";
import type { Card, FileSummary, Message } from "./types";

vi.mock("./lib/ws", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./lib/ws")>()),
  Commands: (await import("./test/fakeSpace")).FakeCommands,
}));

useView();

// A Card that a previous test left waiting for a teardown answer must not keep
// its frame: the row is removed with its view. The retention maps start empty.
beforeEach(() => {
  liveRetained.messages = 0;
  liveRetained.chats = 0;
});

const notes: FileSummary = {
  id: "f1",
  chat: "h1",
  name: "notes.txt",
  media_type: "text/plain",
  size: 5,
  sha256: "ab".repeat(32),
  author: { kind: "user", id: "u1" },
  at: "2026-01-01T00:00:00.000Z",
  seq: "2",
};

// A Bot Message that carries one Card.
function messageWith(card: Card | null, text = "Here is the result."): Message {
  return {
    id: `m${space.messages.length + 1}`,
    chat: space.chat.id,
    author: { kind: "bot", id: space.bot.id },
    acting: "u1",
    text,
    at: "2026-01-01T00:00:01.000Z",
    seq: String(space.messages.length + 2),
    run: null,
    file: null,
    card,
  };
}

// Opens the Chat with one app Card already in its record.
async function openCard(card: Card = appCard, html = appHtml): Promise<View> {
  serveFiles();
  space.showAppCard(card, html);
  return openChat();
}

// The URL a Card's proxy frame is loaded from: the Space's route, carrying the
// Card's effective policy as one encoded query value.
function sandboxSrc(csp: string): string {
  return `/api/mcp/card/sandbox?csp=${encodeURIComponent(csp)}`;
}

// The Card's outer frame: the Space's sandbox proxy route, never the Card's own
// document.
async function frameOf(view: View): Promise<HTMLIFrameElement> {
  await waitFor(() => expect(view.container.querySelector("iframe.app-card__frame")).toBeTruthy());
  return element<HTMLIFrameElement>(view, "iframe.app-card__frame");
}

// One message the proxy frame posts to its host. The frame is sandboxed, so its
// origin serializes to "null"; the source window is the identity the host
// checks.
function fromProxySource(source: MessageEventSource | null, message: unknown) {
  window.dispatchEvent(
    new MessageEvent("message", {
      data: message,
      origin: window.location.origin,
      source,
    }),
  );
}

function fromProxy(frame: HTMLIFrameElement, message: unknown) {
  fromProxySource(frame.contentWindow as MessageEventSource, message);
}

// One JSON-RPC message the Card sent, as the proxy relays it to the host.
function fromCard(frame: HTMLIFrameElement, payload: unknown) {
  fromProxy(frame, { neha: "sandbox-message", payload });
}

// The spy that records what the host sent into the proxy frame.
function spyPosts(frame: HTMLIFrameElement) {
  return vi.spyOn(frame.contentWindow!, "postMessage");
}

type PostSpy = ReturnType<typeof spyPosts>;

// What a spy recorded, in order.
function postsOf(spy: PostSpy) {
  return spy.mock.calls.map(([message]) => message as Record<string, any>);
}

// The JSON-RPC messages the host relayed to the Card.
function postsToApp(spy: PostSpy) {
  return postsOf(spy)
    .filter((message) => message.neha === "sandbox-post")
    .map((message) => message.payload as Record<string, any>);
}

// The one mount notice the host sent the proxy, when it sent it.
function mountNotice(spy: PostSpy) {
  return postsOf(spy).find((message) => message.neha === "mount");
}

// The Card's frame after the proxy said ready and the host handed it the
// document.
async function mountedApp(view: View): Promise<{
  frame: HTMLIFrameElement;
  proxy: MessageEventSource;
  spy: PostSpy;
}> {
  const frame = await frameOf(view);
  const proxy = frame.contentWindow as MessageEventSource;
  const spy = spyPosts(frame);
  fromProxy(frame, { neha: "sandbox-ready" });
  await waitFor(() => expect(mountNotice(spy)).toBeTruthy());
  return { frame, proxy, spy };
}

// Answers the Card's initialize request and its initialized notification, so
// the app is fully mounted and may receive tool data.
async function handshake(frame: HTMLIFrameElement, spy: PostSpy) {
  fromCard(frame, { jsonrpc: "2.0", id: 1, method: "ui/initialize", params: {} });
  await waitFor(() => expect(postsToApp(spy).some((message) => message.id === 1)).toBe(true));
  fromCard(frame, { jsonrpc: "2.0", method: "ui/notifications/initialized", params: {} });
  await waitFor(() =>
    expect(
      postsToApp(spy).some((message) => message.method === "ui/notifications/tool-input"),
    ).toBe(true),
  );
}

// How many refresh Files a test's helper has added, so each event names a File
// the record did not hold before.
let refreshedFiles = 0;

// Makes the record add a File to the open Chat, so the page re-reads it. The
// event is a valid `neha.file.added` for a File the fake Space holds, and the
// File carries no Message: it shows nowhere, so the row under test keeps its
// place rather than gaining a neighbour. Resolves once the refresh's reads
// answered.
async function refreshRecord() {
  const count = (name: string) => space.asked.filter((asked) => asked === name).length;
  const before = { messages: count("chat_read"), files: count("file_list") };
  // A File the Space records for this Chat. The page re-reads the Chat on any
  // record event about it, so this is the refresh a real File addition starts.
  refreshedFiles += 1;
  const file: FileSummary = {
    id: `f-refresh-${refreshedFiles}`,
    chat: space.chat.id,
    name: "refresh.txt",
    media_type: "text/plain",
    size: 1,
    sha256: "ab".repeat(32),
    author: { kind: "user", id: "u1" },
    at: "2026-01-01T00:00:00.000Z",
    seq: String(space.seq + 1),
  };
  space.files = [...space.files, file];
  record("neha.file.added", {
    file: file.id,
    chat: file.chat,
    name: file.name,
    media_type: file.media_type,
    size: file.size,
    sha256: file.sha256,
  });
  await waitFor(() => {
    expect(count("chat_read")).toBeGreaterThan(before.messages);
    expect(count("file_list")).toBeGreaterThan(before.files);
  });
  await drain();
  flush();
  await drain();
}

// Opens the Chat with one app Card already in its record and names the Message
// that carries it, so a test can retire that Message from the record.
async function openCardMessage(): Promise<{ view: View; message: Message }> {
  serveFiles();
  const message = space.showAppCard(appCard, appHtml);
  const view = await openChat();
  return { view, message };
}

// Makes the Chat answer without one of its Messages, as a re-read after the
// record removed it does.
async function forgetMessage(message: Message) {
  space.messages = space.messages.filter((known) => known.id !== message.id);
  await refreshRecord();
}

// The one teardown request the host sent a Card, and every one it sent.
function teardownOf(spy: PostSpy) {
  return postsToApp(spy).find((message) => message.method === "ui/resource-teardown");
}

function teardownsTo(spy: PostSpy) {
  return postsToApp(spy).filter((message) => message.method === "ui/resource-teardown");
}

// Opens the sidebar's second Chat, the way a reader switches Chats.
function switchToOtherChat(view: View) {
  fireEvent.click(element(view, ".conv:nth-of-type(2)"));
}

// The title of the Chat on screen: the pane that is not leaving.
function shownChatTitle(view: View): string {
  return element(view, ".chat-pane:not(.chat-pane--retiring) .chat__titles h1").textContent ?? "";
}

describe("App with an app Card", () => {
  it("mounts the card's raw HTML in the sandbox proxy frame with its policy", async () => {
    const view = await openCard();
    const { frame, spy } = await mountedApp(view);

    // The frame is the Space's proxy route, not the Card's document, and it
    // shares no origin with the Space: scripts run, and nothing else. The route
    // carries the Card's effective policy as one query value, so the proxy can
    // derive the rule the app frame's own navigation is held to.
    expect(frame.getAttribute("src")).toBe(sandboxSrc(DEFAULT_CARD_CSP));
    expect(frame.hasAttribute("srcdoc")).toBe(false);
    expect(frame.getAttribute("sandbox")).toBe("allow-scripts");
    expect(frame.getAttribute("referrerpolicy")).toBe("no-referrer");
    expect(frame.getAttribute("title")).toBe(t("chat.card.title"));
    expect(frame.style.height).toBe("320px");

    // The proxy is handed the Card's exact bytes, unparsed, and the policy the
    // proxy puts in front of them itself. The mount carries no token: the relay
    // is bound to the document by a channel that document owns.
    const mount = mountNotice(spy);
    expect(mount).toEqual({ neha: "mount", html: appHtml, csp: DEFAULT_CARD_CSP });
    // The client never builds the document: the policy is a separate field.
    expect(mount?.html).not.toContain("Content-Security-Policy");
    expect(view.container.querySelector("img")).toBeNull();

    // The Message's own text still reads.
    expect(view.container.querySelector(".msg__text")?.textContent).toBe("Here is the result.");
  });

  it("carries each card's own policy in the proxy's query", async () => {
    serveFiles();
    const framed = "default-src 'none'; frame-src https://embed.example.com";
    space.showAppCard(appCard, appHtml);
    space.showAppCard(
      { ...appCard, file: "f3", csp: framed },
      "<!doctype html><html><body><p>Second card.</p></body></html>",
    );
    const view = await openChat();
    await waitFor(() =>
      expect(view.container.querySelectorAll("iframe.app-card__frame")).toHaveLength(2),
    );
    const frames = Array.from(
      view.container.querySelectorAll<HTMLIFrameElement>("iframe.app-card__frame"),
    );
    const spies = frames.map((frame) => spyPosts(frame));
    for (const frame of frames) fromProxy(frame, { neha: "sandbox-ready" });
    await waitFor(() => {
      for (const spy of spies) expect(mountNotice(spy)).toBeTruthy();
    });

    // Every proxy frame loads with the policy its own Card carries: the default
    // when it names none, and the declared frame-src policy when it does. The
    // proxy reads the app frame's frame rule from that same value.
    expect(frames[0]?.getAttribute("src")).toBe(sandboxSrc(DEFAULT_CARD_CSP));
    expect(frames[1]?.getAttribute("src")).toBe(sandboxSrc(framed));
    expect(mountNotice(spies[0]!)?.csp).toBe(DEFAULT_CARD_CSP);
    expect(mountNotice(spies[1]!)?.csp).toBe(framed);
    const query = new URL(frames[1]!.getAttribute("src")!, "http://space.test").searchParams;
    expect(query.get("csp")).toBe(framed);
  });

  it("relays only the proxy's envelope, never a document's own wrapping", async () => {
    const view = await openCard();
    const { frame, proxy, spy } = await mountedApp(view);

    // The host reads only the proxy's protocol: the app's own wrapping is not a
    // relay, and a raw JSON-RPC frame from the proxy's window is not a relayed
    // Card message either.
    fromProxySource(proxy, {
      __neha: "not-a-relay",
      data: { jsonrpc: "2.0", id: 9, method: "ui/initialize", params: {} },
    });
    fromProxySource(proxy, { jsonrpc: "2.0", id: 6, method: "ui/initialize", params: {} });
    // A malformed relayed payload is dropped without an answer.
    fromCard(frame, "{not json");
    await drain();
    expect(postsToApp(spy)).toEqual([]);

    // The proxy's own envelope still round-trips both ways: the app speaks to
    // the host as a message, and the host answers as a post.
    fromCard(frame, { jsonrpc: "2.0", id: 10, method: "ui/initialize", params: {} });
    await waitFor(() => expect(postsToApp(spy).some((message) => message.id === 10)).toBe(true));
  });

  it("answers initialize, then hands data only after the view's initialized", async () => {
    const view = await openCard();
    const { frame, spy } = await mountedApp(view);

    fromCard(frame, {
      jsonrpc: "2.0",
      id: 1,
      method: "ui/initialize",
      params: { protocolVersion: "2026-01-26", appInfo: { name: "Test card" } },
    });
    await waitFor(() => expect(postsToApp(spy).some((message) => message.id === 1)).toBe(true));

    // Only the initialize answer goes out. It names the host, advertises the
    // capabilities this host implements, and offers no server tools/resources.
    const answered = postsToApp(spy);
    expect(answered.map((message) => message.method ?? "answer")).toEqual(["answer"]);
    const init = answered.find((message) => message.id === 1);
    expect(init?.result).toMatchObject({
      protocolVersion: "2026-01-26",
      hostInfo: { name: "Neha", version: "1" },
      hostCapabilities: { message: { text: {} }, openLinks: {} },
      hostContext: { theme: "light", locale: "en" },
    });
    expect(typeof init?.result.hostInfo.version).toBe("string");
    expect(init?.result.hostCapabilities).not.toHaveProperty("serverTools");
    expect(init?.result.hostCapabilities).not.toHaveProperty("serverResources");
    expect(init?.result.hostContext.containerDimensions.height).toBe(320);

    // The view's own initialized notification opens the handshake; the host
    // never sends one, and only now does the tool's call and its answer arrive.
    fromCard(frame, { jsonrpc: "2.0", method: "ui/notifications/initialized", params: {} });
    await waitFor(() =>
      expect(
        postsToApp(spy).some((message) => message.method === "ui/notifications/tool-input"),
      ).toBe(true),
    );

    const posted = postsToApp(spy);
    expect(posted.map((message) => message.method ?? "answer")).toEqual([
      "answer",
      "ui/notifications/tool-input",
      "ui/notifications/tool-result",
    ]);
    expect(posted.some((message) => message.method === "ui/notifications/initialized")).toBe(false);
    expect(posted[1]?.params).toEqual({ arguments: { query: "hello" } });
    expect(posted[2]?.params).toEqual({
      content: [{ type: "text", text: JSON.stringify({ answer: 42 }) }],
      structuredContent: { answer: 42 },
    });
  });

  it("accepts nothing from the Card's own window or any other window", async () => {
    const view = await openCard();
    const { frame, proxy, spy } = await mountedApp(view);

    // A window that is not the proxy cannot speak for the Card, even with the
    // proxy's own protocol.
    fromProxySource(window, {
      neha: "sandbox-message",
      payload: { jsonrpc: "2.0", id: 5, method: "ui/initialize", params: {} },
    });
    // The host reads only the proxy's protocol: a raw JSON-RPC frame from the
    // proxy's window is not a relayed Card message.
    fromProxySource(proxy, { jsonrpc: "2.0", id: 6, method: "ui/initialize", params: {} });
    // A malformed relayed payload is dropped without an answer.
    fromCard(frame, "{not json");
    await drain();
    expect(postsToApp(spy)).toEqual([]);

    // The real relay still works, so the test's silence is the source check.
    fromCard(frame, { jsonrpc: "2.0", id: 7, method: "ui/initialize", params: {} });
    await waitFor(() => expect(postsToApp(spy).some((message) => message.id === 7)).toBe(true));
  });

  it("relays a Card's message only once the proxy mounted the document", async () => {
    const view = await openCard();
    const frame = await frameOf(view);
    const spy = spyPosts(frame);

    // The proxy relayed a message before the host handed it a document: no app
    // is mounted, so no initialize can be answered, and nothing is kept from
    // it for the app that mounts later.
    fromCard(frame, { jsonrpc: "2.0", id: 1, method: "ui/initialize", params: {} });
    await drain();
    expect(postsToApp(spy)).toEqual([]);

    fromProxy(frame, { neha: "sandbox-ready" });
    await waitFor(() => expect(mountNotice(spy)).toBeTruthy());
    fromCard(frame, { jsonrpc: "2.0", method: "ui/notifications/initialized", params: {} });
    await drain();
    expect(postsToApp(spy)).toEqual([]);

    // The mounted app's own handshake still works: data only after its
    // initialize was answered and the view said it is initialized.
    await handshake(frame, spy);
    expect(postsToApp(spy).map((message) => message.method ?? "answer")).toEqual([
      "answer",
      "ui/notifications/tool-input",
      "ui/notifications/tool-result",
    ]);
  });

  it("sends the proxy plain JSON, which a real postMessage can clone", async () => {
    const view = await openCard();
    const { frame, spy } = await mountedApp(view);
    await handshake(frame, spy);

    // Every control message and relayed payload is plain JSON: no proxy or
    // getter from the host's store is left in it, and it structured-clones the
    // way a real window boundary requires.
    const sent = spy.mock.calls.map(([message]) => message);
    expect(sent.length).toBeGreaterThanOrEqual(4);
    for (const payload of sent) {
      expect(() => structuredClone(payload)).not.toThrow();
      expect(structuredClone(payload)).toEqual(payload);
    }
    const init = postsToApp(spy)[0];
    expect(init?.jsonrpc).toBe("2.0");
    expect(init?.result.hostInfo).toEqual({ name: "Neha", version: "1" });
  });

  it("hands the app the tool result the Space recorded", async () => {
    const recorded = {
      content: [{ type: "text", text: "the answer" }],
      structuredContent: { answer: 42 },
    };
    const view = await openCard({ ...appCard, result: recorded });
    const { frame, spy } = await mountedApp(view);
    await handshake(frame, spy);

    const result = postsToApp(spy).find(
      (message) => message.method === "ui/notifications/tool-result",
    );
    expect(result?.params).toEqual(recorded);
  });

  it("mounts with the policy the card names", async () => {
    const csp = "default-src 'none'; connect-src https://api.example.com";
    const view = await openCard({ ...appCard, csp });
    const { frame, spy } = await mountedApp(view);
    expect(mountNotice(spy)?.csp).toBe(csp);
    // The same policy travels in the proxy's query, so the frame rule the proxy
    // derives is the one this Card declared.
    expect(frame.getAttribute("src")).toBe(sandboxSrc(csp));
  });

  it("refuses a File larger than a Card may be without reading it", async () => {
    const fetchSpy = serveFiles();
    space.showAppCard(appCard, appHtml);
    space.files = space.files.map((file) =>
      file.id === appCard.file ? { ...file, size: MAX_CARD_BYTES + 1 } : file,
    );
    const view = await openChat();

    const state = await waitFor(() => element<HTMLElement>(view, ".app-card__state"));
    expect(state.textContent).toBe(t("chat.card.failed"));
    expect(fetchSpy).not.toHaveBeenCalled();
    expect(view.container.querySelector("iframe")).toBeNull();
  });

  it("grows the frame on a relayed size-changed, within bounds", async () => {
    const view = await openCard();
    const { frame } = await mountedApp(view);
    expect(frame.style.height).toBe("320px");

    fromCard(frame, {
      jsonrpc: "2.0",
      method: "ui/notifications/size-changed",
      params: { width: 480, height: 512 },
    });
    await waitFor(() => expect(frame.style.height).toBe("512px"));

    fromCard(frame, {
      jsonrpc: "2.0",
      method: "ui/notifications/size-changed",
      params: { width: 480, height: 4000 },
    });
    await waitFor(() => expect(frame.style.height).toBe("800px"));

    fromCard(frame, {
      jsonrpc: "2.0",
      method: "ui/notifications/size-changed",
      params: { width: 480, height: 10 },
    });
    await waitFor(() => expect(frame.style.height).toBe("120px"));
  });

  it("relays a card's user message into the Chat", async () => {
    const view = await openCard();
    const { frame, spy } = await mountedApp(view);
    await handshake(frame, spy);

    fromCard(frame, {
      jsonrpc: "2.0",
      id: 7,
      method: "ui/message",
      params: {
        message: { role: "user", content: [{ type: "text", text: "Use the answer 42." }] },
      },
    });

    await waitFor(() =>
      expect(space.sent).toContainEqual({ chat: "h1", text: "Use the answer 42." }),
    );
    await waitFor(() =>
      expect(postsToApp(spy).some((message) => message.id === 7 && "result" in message)).toBe(true),
    );
  });

  it("refuses a card's message while the Chat is busy", async () => {
    serveFiles();
    space.showAppCard(appCard, appHtml);
    // The Space reports the Chat working, so its composer is closed.
    space.statuses = { [space.chat.id]: "working" };
    const view = await openChat();
    const { frame, spy } = await mountedApp(view);
    await handshake(frame, spy);

    fromCard(frame, {
      jsonrpc: "2.0",
      id: 7,
      method: "ui/message",
      params: {
        message: { role: "user", content: [{ type: "text", text: "Use the answer 42." }] },
      },
    });
    await waitFor(() =>
      expect(postsToApp(spy).find((message) => message.id === 7)?.error).toBeTruthy(),
    );

    // The shared send boundary refused it: no second Run reached the Space.
    expect(space.sent).toEqual([]);
    expect(space.asked).not.toContain("message_send");
    // The composer is closed for the same reason.
    expect(element<HTMLTextAreaElement>(view, "textarea").disabled).toBe(true);
  });

  it("tells the app the fixed light theme and a language change while it is open", async () => {
    const view = await openCard();
    const { frame, spy } = await mountedApp(view);
    await handshake(frame, spy);

    // The Space fixes a light theme in `color-scheme`, so the Card is told light.
    const init = postsToApp(spy).find((message) => message.id === 1);
    expect(init?.result.hostContext).toMatchObject({ theme: "light", locale: "en" });

    // The reader switches the interface language while the app is open: the
    // Card hears the new host context instead of staying on the old language.
    flush(() => setLocale("zh-CN"));
    await waitFor(() =>
      expect(
        postsToApp(spy).find(
          (message) => message.method === "ui/notifications/host-context-changed",
        ),
      ).toBeTruthy(),
    );

    const changed = postsToApp(spy).find(
      (message) => message.method === "ui/notifications/host-context-changed",
    );
    expect(changed?.jsonrpc).toBe("2.0");
    expect(changed?.params).toEqual({ theme: "light", locale: "zh" });
  });

  it("opens a safe link and refuses every other scheme", async () => {
    const view = await openCard();
    const { frame, spy } = await mountedApp(view);
    await handshake(frame, spy);
    const open = vi.spyOn(window, "open").mockImplementation(() => null);

    fromCard(frame, {
      jsonrpc: "2.0",
      id: 11,
      method: "ui/open-link",
      params: { url: "javascript:alert(1)" },
    });
    await waitFor(() =>
      expect(postsToApp(spy).find((message) => message.id === 11)?.error.code).toBe(-32602),
    );
    expect(open).not.toHaveBeenCalled();

    for (const url of ["data:text/html,hi", "file:///etc/passwd", "http://example.com/a"]) {
      fromCard(frame, { jsonrpc: "2.0", id: 12, method: "ui/open-link", params: { url } });
    }
    await drain();
    expect(open).not.toHaveBeenCalled();

    fromCard(frame, {
      jsonrpc: "2.0",
      id: 13,
      method: "ui/open-link",
      params: { url: "https://example.com/report?q=1" },
    });
    await waitFor(() =>
      expect(postsToApp(spy).some((message) => message.id === 13 && "result" in message)).toBe(
        true,
      ),
    );
    expect(open).toHaveBeenCalledWith("https://example.com/report?q=1", "_blank", "noopener");

    fromCard(frame, {
      jsonrpc: "2.0",
      id: 14,
      method: "ui/open-link",
      params: { url: "http://127.0.0.1:7337/healthz" },
    });
    await waitFor(() =>
      expect(open).toHaveBeenCalledWith("http://127.0.0.1:7337/healthz", "_blank", "noopener"),
    );
  });

  it("refuses a tool call because the host offers no tools", async () => {
    const view = await openCard();
    const { frame, spy } = await mountedApp(view);
    await handshake(frame, spy);

    fromCard(frame, {
      jsonrpc: "2.0",
      id: 21,
      method: "tools/call",
      params: { name: "anything", arguments: {} },
    });
    await waitFor(() =>
      expect(postsToApp(spy).find((message) => message.id === 21)?.error.code).toBe(-32601),
    );
  });

  it("shows a placeholder for a kind it does not know and keeps the text", async () => {
    space.messages = [messageWith({ kind: "chart", file: null }, "A chart arrived.")];
    const view = await openChat();

    const row = await waitFor(() => element<HTMLElement>(view, ".card-placeholder"));
    expect(row.textContent).toContain(t("chat.card.placeholder"));
    expect(row.textContent).toContain("chart");
    expect(view.container.querySelector("iframe")).toBeNull();
    expect(view.container.querySelector(".msg__text")?.textContent).toBe("A chart arrived.");
  });

  it("never turns an unknown kind into markup", async () => {
    const kind = "<img src=x onerror=alert(1)><script>bad()</script>";
    space.messages = [messageWith({ kind, file: null }, "Something arrived.")];
    const view = await openChat();

    const row = await waitFor(() => element<HTMLElement>(view, ".card-placeholder"));
    expect(row.textContent).toBe(t("chat.card.placeholder"));
    expect(view.container.querySelector("img")).toBeNull();
    expect(view.container.querySelector("script")).toBeNull();
  });

  it("shows one compact line when the card's File cannot be read", async () => {
    serveFiles();
    space.showAppCard(appCard, appHtml);
    // The File is listed, but its bytes are gone.
    space.fileContents = {};
    const view = await openChat();

    const state = await waitFor(() => element<HTMLElement>(view, ".app-card__state"));
    expect(state.textContent).toBe(t("chat.card.failed"));
    expect(state.textContent).not.toContain("api/files");
    expect(view.container.querySelector("iframe")).toBeNull();
  });

  it("shows a File and a Card on the same Message", async () => {
    serveFiles();
    const message = space.showAppCard(appCard, appHtml);
    space.files = [...space.files, notes];
    space.messages = space.messages.map((known) =>
      known === message ? { ...known, file: notes.id } : known,
    );
    const view = await openChat();

    await frameOf(view);
    expect(element(view, ".file-card").getAttribute("href")).toBe("/api/files/f1");
    expect(view.container.querySelectorAll(".app-card")).toHaveLength(1);
  });

  it("keeps the open app's frame when a refresh answers with the same record", async () => {
    const view = await openCard();
    const { frame } = await mountedApp(view);

    await refreshRecord();

    // The row kept its identity, so the frame — and the app running in it — is
    // the very same node, not a remount that lost the app's state.
    expect(view.container.querySelector("iframe.app-card__frame")).toBe(frame);
    expect(frame.isConnected).toBe(true);
    expect(view.container.querySelector(".chat-row--closing")).toBeNull();
  });

  it("orders rows by the snapshot's order after a live event showed a later message first", async () => {
    serveFiles();
    space.files = [appFile];
    space.fileContents = { [appFile.id]: appHtml };
    const view = await openChat();

    // The read the live event starts is held, so it answers without B: the page
    // shows B from the event alone, as a read sent before the record's write
    // does.
    let release: (() => void) | undefined;
    let held = false;
    space.chatDelay = () => {
      if (held) return Promise.resolve();
      held = true;
      return new Promise<void>((resolve) => (release = resolve));
    };
    record(
      "neha.message.added",
      { chat: "h1", message: "mb", text: "Later.", file: null, run: null, card: appCard },
      { kind: "bot", id: "b1" },
    );
    // The record's own B is byte for byte the live one, so the snapshot keeps
    // the row's identity when it names B.
    const later: Message = {
      id: "mb",
      chat: "h1",
      author: { kind: "bot", id: "b1" },
      acting: "u1",
      text: "Later.",
      at: "2026-01-01T00:00:00.000Z",
      seq: String(space.seq),
      run: null,
      file: null,
      card: appCard,
    };
    await waitFor(() => expect(release).toBeDefined());
    release?.();
    space.chatDelay = null;
    await waitFor(() => expect(view.container.textContent).toContain("Later."));
    const frame = await waitFor(() => {
      const found = view.container.querySelector<HTMLIFrameElement>("iframe.app-card__frame");
      expect(found).toBeTruthy();
      return found!;
    });
    const row = frame.closest(".chat-row");

    // The authoritative snapshot arrives naming an earlier Message first. Its
    // order is the stream's, and B's row — with the app's frame inside it — is
    // the very same node, now after the earlier Message.
    const earlier: Message = {
      id: "ma",
      chat: "h1",
      author: { kind: "user", id: "u1" },
      acting: null,
      text: "Earlier.",
      at: "2025-12-31T23:00:00.000Z",
      seq: "90",
      run: null,
      file: null,
    };
    space.messages = [earlier, later];
    await refreshRecord();

    await waitFor(() =>
      expect(
        Array.from(view.container.querySelectorAll(".msg__text")).map((node) => node.textContent),
      ).toEqual(["Earlier.", "Later."]),
    );
    expect(view.container.querySelector("iframe.app-card__frame")).toBe(frame);
    expect(frame.closest(".chat-row")).toBe(row);
  });

  it("replaces the frame when a refresh changes the Message", async () => {
    const view = await openCard();
    const { frame } = await mountedApp(view);

    space.messages = space.messages.map((message) => ({
      ...message,
      text: "The result was updated.",
    }));
    await refreshRecord();
    await waitFor(() =>
      expect(view.container.querySelector(".msg__text")?.textContent).toBe(
        "The result was updated.",
      ),
    );

    const replacement = element<HTMLIFrameElement>(view, "iframe.app-card__frame");
    expect(replacement).not.toBe(frame);
  });

  it("gives the frame up when the mounted document navigates", async () => {
    const view = await openCard();
    const { frame, proxy, spy } = await mountedApp(view);
    await handshake(frame, spy);
    const relayed = postsToApp(spy).length;

    fromProxySource(proxy, { neha: "sandbox-navigation" });

    // The relay is revoked for good, the frame goes, and the Card shows its
    // quiet failure line instead of handing the new document the tool's data.
    await waitFor(() =>
      expect(element<HTMLElement>(view, ".app-card__state").textContent).toBe(
        t("chat.card.failed"),
      ),
    );
    expect(view.container.querySelector("iframe")).toBeNull();

    fromCard(frame, { jsonrpc: "2.0", id: 2, method: "ui/initialize", params: {} });
    await drain();
    expect(postsToApp(spy)).toHaveLength(relayed);
  });

  it("keeps a retired Card's row and frame in place until the app answers teardown", async () => {
    const { view, message } = await openCardMessage();
    const { frame, spy } = await mountedApp(view);
    await handshake(frame, spy);

    await forgetMessage(message);

    // The Chat asks the app to tear its resource down, as a JSON-RPC request
    // with an id that goes through the proxy relay like any other host message.
    const teardown = await waitFor(() => {
      const asked = teardownOf(spy);
      expect(asked).toBeTruthy();
      return asked!;
    });
    expect(teardown.jsonrpc).toBe("2.0");
    expect(typeof teardown.id).toBe("number");
    expect(teardown.params).toEqual({});

    // The Message left the list, but its row and frame are still attached: the
    // row is hidden and in its place, never moved, because a moved frame could
    // never answer the request.
    const row = frame.closest(".chat-row");
    expect(row).toBeTruthy();
    expect(row?.classList.contains("chat-row--closing")).toBe(true);
    expect(row?.getAttribute("aria-hidden")).toBe("true");
    expect(row?.contains(frame)).toBe(true);
    expect(frame.isConnected).toBe(true);
    expect(row?.parentElement?.classList.contains("chat__stream")).toBe(true);
    // The retired row is the only one, and it is still the stream's row.
    expect(view.container.querySelectorAll(".chat-row")).toHaveLength(1);

    // A delayed acknowledgement takes the row — and only then the frame — away
    // before the bounded wait is over.
    await new Promise((resolve) => setTimeout(resolve, 25));
    fromCard(frame, { jsonrpc: "2.0", id: teardown.id, result: {} });
    await waitFor(() => expect(frame.isConnected).toBe(false));
    expect(row?.isConnected).toBe(false);
    expect(view.container.querySelector(".chat-row--closing")).toBeNull();
    // One request was sent; the cleanup that followed sent no second one.
    expect(teardownsTo(spy)).toHaveLength(1);
  });

  it("keeps a sibling Card's frame when another Card's row is retired", async () => {
    serveFiles();
    const retired = space.showAppCard(appCard, appHtml);
    space.showAppCard({ ...appCard, file: "f3" }, appHtml);
    const view = await openChat();
    await waitFor(() =>
      expect(view.container.querySelectorAll("iframe.app-card__frame")).toHaveLength(2),
    );
    const frames = Array.from(
      view.container.querySelectorAll<HTMLIFrameElement>("iframe.app-card__frame"),
    );
    const spies = frames.map((frame) => spyPosts(frame));
    for (const frame of frames) fromProxy(frame, { neha: "sandbox-ready" });
    await waitFor(() => {
      for (const spy of spies) expect(mountNotice(spy)).toBeTruthy();
    });

    await forgetMessage(retired);

    // The retired row keeps its place in the stream, so the sibling row is
    // never re-inserted and its frame — with the app running in it — is the
    // very same node.
    expect(frames[0]!.isConnected).toBe(true);
    expect(frames[1]!.isConnected).toBe(true);
    expect(view.container.querySelectorAll("iframe.app-card__frame")[1]).toBe(frames[1]);

    // Once the retired app answers, only its row leaves; the sibling stays.
    const teardown = await waitFor(() => {
      const asked = teardownOf(spies[0]!);
      expect(asked).toBeTruthy();
      return asked!;
    });
    fromCard(frames[0]!, { jsonrpc: "2.0", id: teardown.id, result: {} });
    await waitFor(() => expect(frames[0]!.isConnected).toBe(false));
    const remaining = view.container.querySelectorAll("iframe.app-card__frame");
    expect(remaining).toHaveLength(1);
    expect(remaining[0]).toBe(frames[1]);
  });

  it("keeps the previous Chat's card frame until the app answers teardown", async () => {
    serveFiles();
    space.chats = [space.chat, space.otherChat];
    space.showAppCard(appCard, appHtml);
    const view = await openChat();
    const { frame, spy } = await mountedApp(view);
    await handshake(frame, spy);

    switchToOtherChat(view);
    await waitFor(() => expect(shownChatTitle(view)).toBe("Quill"));

    // The Chat the reader left is still mounted, hidden: its app's document must
    // stay attached to answer the host's teardown request, so the view is not
    // unmounted with the switch.
    const pane = await waitFor(() => {
      const found = view.container.querySelector<HTMLElement>(".chat-pane--retiring");
      expect(found).toBeTruthy();
      return found!;
    });
    expect(pane.getAttribute("aria-hidden")).toBe("true");
    expect(pane.contains(frame)).toBe(true);
    expect(frame.isConnected).toBe(true);

    const teardown = await waitFor(() => {
      const asked = teardownOf(spy);
      expect(asked).toBeTruthy();
      return asked!;
    });
    expect(teardown.jsonrpc).toBe("2.0");

    // A delayed acknowledgement keeps the frame alive; the answer is what lets
    // the outgoing view — and the frame inside it — go.
    await new Promise((resolve) => setTimeout(resolve, 25));
    expect(frame.isConnected).toBe(true);
    fromCard(frame, { jsonrpc: "2.0", id: teardown.id, result: {} });
    await waitFor(() => expect(frame.isConnected).toBe(false));
    expect(view.container.querySelector(".chat-pane--retiring")).toBeNull();
    expect(shownChatTitle(view)).toBe("Quill");
  });

  it("keeps a leaving Chat's frame when the reader goes back to the list", async () => {
    serveFiles();
    space.showAppCard(appCard, appHtml);
    const view = await openChat();
    const { frame, spy } = await mountedApp(view);
    await handshake(frame, spy);

    fireEvent.click(element(view, ".chat__back"));
    await waitFor(() => expect(view.container.querySelector(".select-chat")).toBeTruthy());

    // Leaving to the list is a switch too: the Chat's view stays mounted, hidden,
    // until its app answered the teardown request.
    const pane = view.container.querySelector<HTMLElement>(".chat-pane--retiring");
    expect(pane).toBeTruthy();
    expect(pane?.contains(frame)).toBe(true);
    expect(frame.isConnected).toBe(true);
    const teardown = await waitFor(() => {
      const asked = teardownOf(spy);
      expect(asked).toBeTruthy();
      return asked!;
    });
    fromCard(frame, { jsonrpc: "2.0", id: teardown.id, result: {} });
    await waitFor(() => expect(frame.isConnected).toBe(false));
    expect(view.container.querySelector(".chat-pane--retiring")).toBeNull();
    expect(view.container.querySelector(".select-chat")).toBeTruthy();
  });

  it("switches to a Chat with no card at once", async () => {
    space.chats = [space.chat, space.otherChat];
    const view = await openChat();

    switchToOtherChat(view);
    await waitFor(() => expect(shownChatTitle(view)).toBe("Quill"));

    // Nothing in the Chat the reader left had a document to close, so no view
    // waits behind the switch.
    expect(view.container.querySelector(".chat-pane--retiring")).toBeNull();
    expect(view.container.querySelectorAll(".chat-pane")).toHaveLength(1);
  });

  it("removes a retired row after the bounded wait when the app never answers", async () => {
    const { view, message } = await openCardMessage();
    const { frame, spy } = await mountedApp(view);
    await handshake(frame, spy);
    await forgetMessage(message);
    const teardown = await waitFor(() => {
      const asked = teardownOf(spy);
      expect(asked).toBeTruthy();
      return asked!;
    });
    expect(frame.isConnected).toBe(true);

    // The document never answers: the row leaves once the host's bounded wait
    // is over, so a broken app cannot hold its row forever.
    await new Promise((resolve) => setTimeout(resolve, 550));
    await waitFor(() => expect(frame.isConnected).toBe(false));
    expect(teardownOf(spy)?.id).toBe(teardown.id);
    expect(teardownsTo(spy)).toHaveLength(1);
  });

  it("drops a placeholder row at once when its Message leaves", async () => {
    space.messages = [messageWith({ kind: "chart", file: null }, "A chart arrived.")];
    const view = await openChat();
    await waitFor(() => expect(element(view, ".card-placeholder")).toBeTruthy());

    // A kind this client cannot show has no document to tear down: its row may
    // leave with the Message, unlike an app's row.
    space.messages = [];
    await refreshRecord();
    await waitFor(() => expect(view.container.querySelector(".card-placeholder")).toBeNull());
    expect(view.container.querySelectorAll(".chat-row")).toHaveLength(0);
  });

  it("drops a retired row at once when its Card never mounted", async () => {
    const { view, message } = await openCardMessage();
    // The proxy never said ready, so the host never handed it a document: there
    // is nothing to ask to tear down, and the row may leave at once.
    const frame = await frameOf(view);
    const spy = spyPosts(frame);
    await forgetMessage(message);

    await waitFor(() => expect(frame.isConnected).toBe(false));
    expect(teardownsTo(spy)).toEqual([]);
  });

  it("asks a Card to tear down once when the whole Chat closes", async () => {
    const view = await openCard();
    const { frame, spy } = await mountedApp(view);
    await handshake(frame, spy);

    view.unmount();

    // The frame is leaving with its page, so the host tells the app once and
    // does not wait: nothing could answer a request after the frame is gone.
    const teardowns = teardownsTo(spy);
    expect(teardowns).toHaveLength(1);
    expect(teardowns[0]?.jsonrpc).toBe("2.0");
    expect(teardowns[0]?.id).toBeTruthy();
  });

  it("previews a Card-only Message as an app in the sidebar", async () => {
    // A finished Message is already the Chat's preview.
    space.messages = [messageWith(null, "first preview")];
    const view = await openChat();
    await waitFor(() =>
      expect(element<HTMLElement>(view, ".conv__preview").textContent).toBe("first preview"),
    );

    // A Card-only Message has no text: the preview keeps the card flag, so the
    // sidebar says the app arrived instead of drawing a blank line.
    const cardOnly = messageWith({ kind: "app", file: null }, "");
    space.announceCard(cardOnly);
    await waitFor(() =>
      expect(element<HTMLElement>(view, ".conv__preview").textContent).toBe(t("sidebar.app")),
    );
  });

  it("merges a live Message and its Card that a stale read predates", async () => {
    serveFiles();
    space.files = [appFile];
    space.fileContents = { [appFile.id]: appHtml };
    const view = await openChat();

    // The record never lists this Message: the event is the only word of it, as
    // an answer to a read sent before the record's write can be. The read is
    // held open so the event lands first and the answer still lacks the Message.
    let release: (() => void) | undefined;
    let held = false;
    space.chatDelay = () => {
      if (held) return Promise.resolve();
      held = true;
      return new Promise<void>((resolve) => (release = resolve));
    };
    const live = messageWith(appCard, "A card arrived.");
    space.announceCard(live);
    await waitFor(() => expect(release).toBeDefined());
    release?.();
    space.chatDelay = null;

    // The read answers without the Message, but the live event named it first:
    // the Chat keeps the Message and its Card rather than hiding both.
    await waitFor(() => expect(view.container.textContent).toContain("A card arrived."));
    await waitFor(() =>
      expect(view.container.querySelector("iframe.app-card__frame")).toBeTruthy(),
    );
    expect(space.messages.some((message) => message.id === live.id)).toBe(false);
    // The copy is held for the read that may still be missing it.
    expect(liveRetained.messages).toBe(1);
  });

  it("retires a live copy once a read answers with the Message", async () => {
    serveFiles();
    space.files = [appFile];
    space.fileContents = { [appFile.id]: appHtml };
    const view = await openChat();

    // The record added the Message while a read was on its way.
    let release: (() => void) | undefined;
    let held = false;
    space.chatDelay = () => {
      if (held) return Promise.resolve();
      held = true;
      return new Promise<void>((resolve) => (release = resolve));
    };
    const live = messageWith(appCard, "A card arrived.");
    space.announceCard(live);
    await waitFor(() => expect(release).toBeDefined());
    release?.();
    space.chatDelay = null;
    await waitFor(() => expect(liveRetained.messages).toBe(1));

    // The next read answers with the Message: the live copy is no longer
    // needed, so the page retires it instead of holding it forever.
    space.messages = [...space.messages, live];
    await refreshRecord();
    expect(liveRetained.messages).toBe(0);
    expect(view.container.textContent).toContain("A card arrived.");
  });

  it("keeps no live Message for a Chat the page is not reading", async () => {
    space.chats = [space.chat, space.otherChat];
    const view = await openChat();

    // Another Chat's Message arrives live. The page is not reading that Chat,
    // so nothing about it may be kept — it could never be retired by an answer.
    space.announceCard({
      ...messageWith(appCard, "An other-chat card."),
      id: "m9",
      chat: "h2",
      author: { kind: "bot", id: space.otherBot.id },
    });
    await drain();
    expect(liveRetained.messages).toBe(0);

    // Opening that Chat reads an empty record: the live Message must not
    // surface in its stream. (The sidebar preview does name it, as it names any
    // new Message, but the Chat itself never received it.)
    fireEvent.click(element(view, ".conv:nth-of-type(2)"));
    await waitFor(() => expect(element(view, ".chat__titles h1").textContent).toBe("Quill"));
    await drain();
    expect(element<HTMLElement>(view, ".chat__stream").textContent).not.toContain(
      "An other-chat card.",
    );
    expect(view.container.querySelector(".msg__text")).toBeNull();
  });

  it("keeps a Card that only a live message event named", async () => {
    serveFiles();
    space.files = [appFile];
    space.fileContents = { [appFile.id]: appHtml };
    // The read answers with the Message, not yet its Card, as a read sent
    // before the record wrote the Card can.
    const message = messageWith(null, "A card arrived.");
    space.messages = [message];
    const view = await openChat();
    expect(view.container.querySelector("iframe")).toBeNull();

    space.announceCard({ ...message, card: appCard });
    await waitFor(() =>
      expect(view.container.querySelector("iframe.app-card__frame")).toBeTruthy(),
    );
  });
});
