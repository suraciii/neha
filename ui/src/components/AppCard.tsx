import { Show, createEffect, createSignal, onCleanup, onSettled } from "solid-js";

import { locale, t } from "../i18n";
import {
  CARD_HOST_CAPABILITIES,
  CARD_HOST_INFO,
  CARD_PROTOCOL_VERSION,
  INITIAL_CARD_HEIGHT,
  cardCsp,
  cardLink,
  cardMessageText,
  cardSandboxUrl,
  cardTheme,
  cardToolResult,
  clampCardHeight,
  loadCardHtml,
  plainJson,
} from "../lib/cards";
import type { Card, FileSummary } from "../types";

interface AppCardProps {
  card: Card;
  // The File that holds the Card's HTML, as the Chat lists it.
  file: FileSummary | null;
  // Puts the text a Card asks for in the Chat, through the Chat's own send.
  onSend: (text: string) => Promise<boolean>;
  // The Chat retired this Card's row: the row is hidden but stays attached, and
  // the host asks the app to tear its resource down. The app's answer (or the
  // bounded wait) closes the row, and only then may `onClosed` take it away.
  closing?: boolean;
  onClosed?: () => void;
}

// One request or notification a Card sends its host.
interface CardMessage {
  id?: unknown;
  method: string;
  params?: unknown;
}

// One JSON-RPC message the host sends the Card's frame. A notification leaves
// the id out, and a request carries it; JSON drops the undefined ones.
interface CardFrameMessage {
  id?: unknown;
  method?: string;
  params?: unknown;
  result?: unknown;
  error?: { code: number; message: string };
}

// What the proxy says to the host: that its route loaded, one message the Card
// sent, or the one word that the Card's inner document navigated away from what
// the host mounted. The relay stops at that word.
interface ProxyReady {
  neha: "sandbox-ready";
}
interface ProxyMessage {
  neha: "sandbox-message";
  payload: unknown;
}
interface ProxyNavigation {
  neha: "sandbox-navigation";
}

// The document the host hands the proxy once it is ready: the Card's exact
// bytes, unparsed, and the policy the proxy prepends as a meta tag. The proxy's
// relay back to the host is bound to the document it mounts by a channel that
// document owns, so the mount carries no secret of its own.
interface ProxyMount {
  neha: "mount";
  html: string;
  csp: string;
}

// One host→Card JSON-RPC message, relayed by the proxy into the Card's frame.
interface ProxyPost {
  neha: "sandbox-post";
  payload: unknown;
}

// The host context a Card reads, per the MCP Apps schema.
interface CardHostContext {
  theme: "light";
  locale: string;
  containerDimensions: { width: number; height: number };
}

// How long the host keeps a retired Card's row attached while it waits for the
// app's answer to the teardown request. A document that never answers must not
// hold its row forever, so the wait is bounded; the answer closes it sooner.
const TEARDOWN_ACK_MS = 500;

// The host's teardown requests, numbered so an answer names the one Card it is
// for.
let teardownSeq = 0;

// The language a Card hears: the Space's interface ships two, and the MCP Apps
// schema names a BCP 47 tag.
function cardLocale(): string {
  return locale() === "zh-CN" ? "zh" : "en";
}

// A parsed JSON-RPC frame, before it is known to be a request or a answer.
function parseFrame(data: unknown): Record<string, unknown> | null {
  let frame = data;
  if (typeof frame === "string") {
    try {
      frame = JSON.parse(frame) as unknown;
    } catch {
      return null;
    }
  }
  if (!frame || typeof frame !== "object" || Array.isArray(frame)) return null;
  return frame as Record<string, unknown>;
}

// An object a Card sent, or an empty one.
function objectOf(value: unknown): Record<string, unknown> | null {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

// The proxy's protocol message a window message is, when it is one. A window
// message that is not the proxy's protocol is not part of the relay.
type ProxyNotice = ProxyReady | ProxyMessage | ProxyNavigation;

function proxyNoticeOf(value: unknown): ProxyNotice | null {
  const notice = objectOf(value);
  if (!notice) return null;
  switch (notice.neha) {
    case "sandbox-ready":
    case "sandbox-message":
    case "sandbox-navigation":
      return notice as unknown as ProxyNotice;
    default:
      return null;
  }
}

// One interactive Card, rendered in a frame that shares no origin with the
// Space. The host owns the frame: it answers the Card's JSON-RPC messages and
// relays nothing of its own session into it.
export default function AppCard(props: AppCardProps) {
  let container: HTMLDivElement | undefined;
  let frame: HTMLIFrameElement | undefined;
  // The Card's HTML exactly as the Space served it, and the policy it runs
  // under. The host never parses or rewrites the document: the proxy prepends
  // the policy meta itself.
  const [source, setSource] = createSignal<{ html: string; csp: string } | null>(null);
  const [failed, setFailed] = createSignal(false);
  const [height, setHeight] = createSignal(INITIAL_CARD_HEIGHT);
  // The proxy answered ready, so it may be handed the Card's document.
  const [proxyReady, setProxyReady] = createSignal(false);

  // The view answered initialize and may now receive the tool's data. Only the
  // view's own `ui/notifications/initialized` starts that: the notification is
  // view→host, so the host never sends one.
  let answered = false;
  let ready = false;
  // The document was handed to the proxy, so its JSON-RPC messages may cross.
  let mounted = false;
  // The Card's inner document navigated: the relay is dead for good, and no
  // further app data may reach that frame.
  let revoked = false;
  // The language the Card last heard, so a change is told once.
  let toldLocale: string | null = null;
  // The teardown request the host sent for this Card's document, if it sent
  // one: the request id an answer must name, and the row-removal wait. The
  // request goes at most once, whether the Chat retired the row (`closing`) or
  // the whole frame is leaving with its page.
  let teardownAsked = false;
  let teardownId = 0;
  let teardownTimer = 0;
  // The app answered (or the bounded wait ended), so its row may leave.
  let closed = false;
  // The document the frame currently shows: its File, size, and policy. The page
  // may re-announce the same one — a refresh answering with the same record, or
  // this Chat's view frozen while it closes — and that must not read the bytes
  // again or reset the app.
  let shownDocument: string | null = null;

  // Sends one control message to the proxy frame. The proxy is sandboxed, so
  // its origin is opaque; the source window is the identity the host checks on
  // the way back, and the target accepts any origin.
  const tellProxy = (message: ProxyMount | ProxyPost) => {
    frame?.contentWindow?.postMessage(message, "*");
  };

  // Relays one JSON-RPC message to the Card through the proxy. Only a mounted
  // document hears it, and never one that navigated away from what was mounted.
  const post = (message: CardFrameMessage) => {
    if (!mounted || revoked) return;
    tellProxy({
      neha: "sandbox-post",
      // A Card's frame receives plain JSON. The host's own state is a Solid
      // store, whose objects are proxies a real browser's postMessage refuses
      // with a DataCloneError, even though a test's spy sees nothing.
      payload: plainJson({ jsonrpc: "2.0", ...message }),
    });
  };
  const answer = (id: unknown, result: unknown) => post({ id, result });
  const refuse = (id: unknown, code: number, message: string) =>
    post({ id, error: { code, message } });

  // Hands the Card's document to the proxy once both the bytes and the route
  // are there. The bytes are the Card's own: the proxy is what puts the policy
  // in front of them.
  createEffect(
    () => ({ card: source(), ready: proxyReady() }),
    ({ card, ready }) => {
      if (!card || !ready || mounted || revoked) return;
      mounted = true;
      tellProxy({ neha: "mount", html: card.html, csp: card.csp });
    },
  );

  // Reads a Card's HTML once its Message names the File that holds it. A Card
  // whose File never arrives, or whose bytes are gone, shows one quiet line.
  createEffect(
    () => ({
      id: props.card.file ?? null,
      size: props.file?.size ?? null,
      csp: props.card.csp ?? null,
    }),
    ({ id, size, csp }) => {
      const document = JSON.stringify([id, size, csp]);
      if (document === shownDocument) return;
      shownDocument = document;
      // A new document starts a new frame, a new relay, and a new handshake.
      setSource(null);
      setFailed(false);
      setProxyReady(false);
      mounted = false;
      revoked = false;
      answered = false;
      ready = false;
      toldLocale = null;
      if (id === null) {
        setFailed(true);
        return;
      }
      let current = true;
      void loadCardHtml(id, size).then(
        (html) => {
          if (!current) return;
          // The exact bytes, with no tag prepended and nothing parsed: the
          // proxy is what puts the policy meta in front of them.
          setSource({ html, csp: cardCsp(csp) });
        },
        () => {
          if (!current) return;
          setFailed(true);
        },
      );
      return () => {
        current = false;
      };
    },
  );

  function containerWidth(): number {
    const width = container?.getBoundingClientRect().width ?? 0;
    return Math.max(0, Math.round(width));
  }

  // What a Card reads about its host: the Space's one light theme, the reader's
  // language, and the size the frame occupies. The Space fixes a light theme in
  // `color-scheme`, so the Card is never told the host OS's dark preference.
  function hostContext(): CardHostContext {
    return {
      theme: cardTheme(),
      locale: cardLocale(),
      containerDimensions: { width: containerWidth(), height: height() },
    };
  }

  // Tells the Card the reader's language when it changes while the Card is
  // open, so an open app does not stay in the previous one.
  function tellLocale(current: string) {
    if (!ready || current === toldLocale) return;
    toldLocale = current;
    post({
      method: "ui/notifications/host-context-changed",
      params: { theme: cardTheme(), locale: current },
    });
  }
  createEffect(cardLocale, tellLocale);

  // Answers the Card's initialize request. The Host offers the Card no tools
  // and no resources of its own, so it advertises neither. The tool's call and
  // its answer wait for the view's own initialized notification.
  function initialize(id: unknown) {
    const context = hostContext();
    answered = true;
    ready = false;
    toldLocale = context.locale;
    answer(id, {
      protocolVersion: CARD_PROTOCOL_VERSION,
      hostInfo: { ...CARD_HOST_INFO },
      hostCapabilities: { ...CARD_HOST_CAPABILITIES },
      hostContext: context,
    });
  }

  // The view is initialized, so it can read the tool's call and its answer in
  // the order the model made them.
  function handOff() {
    if (!answered || ready) return;
    ready = true;
    post({
      method: "ui/notifications/tool-input",
      params: { arguments: props.card.input },
    });
    post({
      method: "ui/notifications/tool-result",
      params: cardToolResult(props.card),
    });
    // The reader may have changed the language while the handshake ran.
    tellLocale(cardLocale());
  }

  function dispatch(message: CardMessage) {
    const params = objectOf(message.params) ?? {};
    const id = message.id;
    // A notification has no id, so only a request is answered.
    const asked = id !== undefined && id !== null;
    switch (message.method) {
      case "ui/initialize":
        if (asked) initialize(id);
        return;
      case "ui/notifications/initialized":
        // The view finished initializing; only now may it receive the tool's
        // data. The notification is never answered.
        handOff();
        return;
      case "ui/notifications/size-changed": {
        const next = params.height;
        if (typeof next === "number" && Number.isFinite(next)) setHeight(clampCardHeight(next));
        return;
      }
      case "tools/call":
        // The host advertises no server tools, so a Card cannot call one.
        if (asked) refuse(id, -32601, "the host offers no tools");
        return;
      case "ui/open-link": {
        if (!asked) return;
        const link = cardLink(params.url);
        if (link === null) {
          refuse(id, -32602, "the host does not open this link");
          return;
        }
        window.open(link, "_blank", "noopener");
        answer(id, {});
        return;
      }
      case "ui/message": {
        if (!asked) return;
        const text = cardMessageText(params);
        if (text === null) {
          refuse(id, -32602, "the card sent no user text");
          return;
        }
        void props.onSend(text).then(
          (sent) => {
            if (sent) answer(id, {});
            else refuse(id, -32603, "the Chat did not take the message");
          },
          () => refuse(id, -32603, "the Chat did not take the message"),
        );
        return;
      }
      default:
        // An unknown notification is ignored; an unknown request is refused.
        if (asked) refuse(id, -32601, "method not found");
    }
  }

  // Takes one JSON-RPC frame the proxy relayed from the Card. The proxy has
  // already checked that the inner document is the one it mounted, so the host
  // only validates the frame itself.
  function receiveFrame(raw: Record<string, unknown>) {
    const id = raw.id;
    const asked = id !== undefined && id !== null;
    if (raw.jsonrpc !== "2.0") {
      if (asked) refuse(id, -32600, "invalid request");
      return;
    }
    if (typeof raw.method !== "string") {
      // A response to a request the host sent needs no answer; anything else
      // that names an id is malformed as a request.
      if (asked && !("result" in raw) && !("error" in raw)) refuse(id, -32600, "invalid request");
      return;
    }
    dispatch({ id, method: raw.method, params: raw.params });
  }

  // The Card's inner document navigated away from what the host mounted: the
  // proxy stops relaying, the host never hands the new document the Card's
  // data, and the frame gives way to the quiet failure line.
  function revoke() {
    if (revoked) return;
    revoked = true;
    setProxyReady(false);
    setFailed(true);
  }

  // Lets the retired row leave: the app answered the teardown request, or the
  // bounded wait ended. Only the Chat takes the row out of the stream, and only
  // from here, so the frame stays attached (and the app alive) throughout the
  // handshake.
  function finishClose() {
    if (closed) return;
    closed = true;
    if (teardownTimer !== 0) {
      window.clearTimeout(teardownTimer);
      teardownTimer = 0;
    }
    props.onClosed?.();
  }

  // Asks the mounted app to tear its resource down, at most once. The request is
  // a JSON-RPC request with an id, sent through the proxy relay, and the app's
  // answer comes back through the same relay. A Card that never mounted, or
  // whose document navigated away, has no document to hear it: its row may leave
  // at once.
  function requestTeardown() {
    if (teardownAsked || closed) return;
    teardownAsked = true;
    const proxy = frame;
    if (!proxy || !mounted || revoked || !proxy.contentWindow) {
      finishClose();
      return;
    }
    teardownId = ++teardownSeq;
    teardownTimer = window.setTimeout(finishClose, TEARDOWN_ACK_MS);
    tellProxy({
      neha: "sandbox-post",
      payload: plainJson({ jsonrpc: "2.0", id: teardownId, method: "ui/resource-teardown", params: {} }),
    });
  }

  // The Chat retired this Card's row: keep the frame attached (its row is
  // hidden in place, not moved) until the app answers or the bounded wait ends.
  // The Chat's own removal of a Message is what drives this, never the
  // component's cleanup.
  createEffect(
    () => props.closing === true,
    (closing) => {
      if (closing) requestTeardown();
    },
  );

  // Takes one message from the proxy's window. The relay is the only way a
  // Card's words reach the host: an inner document has no line to it, so a
  // document that navigates cannot keep speaking. The proxy's frame is the one
  // identity the host checks, and the relay only exists once the proxy mounted
  // the document: a word left over from a replaced frame must reach neither the
  // host's handshake nor the Card that replaced it.
  function receive(event: MessageEvent) {
    const proxy = frame?.contentWindow;
    if (!proxy || event.source !== proxy) return;
    const notice = proxyNoticeOf(event.data);
    if (!notice) return;
    switch (notice.neha) {
      case "sandbox-ready":
        // A ready from a frame that is being replaced arrives while no
        // document is mounted here: only the current frame's ready counts.
        if (!revoked && source() !== null) setProxyReady(true);
        return;
      case "sandbox-message": {
        if (!mounted || revoked) return;
        const raw = parseFrame(notice.payload);
        if (raw === null) return;
        // The app's answer to the host's one teardown request names its id and
        // ends the host's wait; it is not part of the app's own protocol.
        if (
          teardownAsked &&
          !closed &&
          raw.id === teardownId &&
          typeof raw.method !== "string" &&
          ("result" in raw || "error" in raw)
        ) {
          finishClose();
          return;
        }
        receiveFrame(raw);
        return;
      }
      case "sandbox-navigation":
        if (!mounted) return;
        revoke();
        return;
    }
  }

  onSettled(() => {
    const listener = (event: MessageEvent) => receive(event);
    window.addEventListener("message", listener);
    return () => window.removeEventListener("message", listener);
  });

  // The frame is leaving with its page before the Chat retired the row (the
  // reader left the Chat, or the record replaced the Message). Its document
  // cannot answer once the frame is gone, so the host only tells it once, and
  // does not wait. The normal path is `closing`, which keeps the row in place
  // until the app answers.
  onCleanup(() => {
    if (teardownTimer !== 0) {
      window.clearTimeout(teardownTimer);
      teardownTimer = 0;
    }
    if (teardownAsked || closed) return;
    const proxy = frame;
    if (!proxy || !mounted || revoked || !proxy.contentWindow) return;
    teardownAsked = true;
    tellProxy({
      neha: "sandbox-post",
      payload: plainJson({ jsonrpc: "2.0", id: ++teardownSeq, method: "ui/resource-teardown", params: {} }),
    });
  });

  return (
    <div class="app-card" ref={container}>
      <Show
        when={!failed()}
        fallback={
          <p class="app-card__state" role="status">
            {t("chat.card.failed")}
          </p>
        }
      >
        <Show
          when={source()}
          fallback={
            <p class="app-card__state" role="status">
              {t("chat.card.loading")}
            </p>
          }
        >
          {(_source) => (
            <iframe
              ref={frame}
              class="app-card__frame"
              title={t("chat.card.title")}
              sandbox="allow-scripts"
              referrerpolicy="no-referrer"
              // The route carries the Card's effective policy, so the proxy can
              // hold the app frame's own navigation to the frame rule the Card
              // declared (or to none at all).
              src={cardSandboxUrl(cardCsp(props.card.csp))}
              style={{ height: `${height()}px` }}
            />
          )}
        </Show>
      </Show>
    </div>
  );
}
