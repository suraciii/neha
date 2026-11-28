import { createEffect, createMemo, createSignal, For, Show } from "solid-js";

import { locale, t } from "../i18n";
import { cardFile } from "../lib/cards";
import { formatBytes } from "../lib/files";
import { dayLabel, timeOfDay } from "../lib/format";
import type {
  Account,
  Approval,
  Bot,
  ChatStatus,
  Computer,
  FileSummary,
  Message,
  RunActivity,
  RunProgress,
} from "../types";
import ActivityCard from "./ActivityCard";
import AppCard from "./AppCard";
import ApprovalCard from "./ApprovalCard";
import Avatar from "./Avatar";
import CardPlaceholder from "./CardPlaceholder";
import FileCard from "./FileCard";
import Icon from "./Icon";

interface ChatViewProps {
  account: Account;
  bot: Bot | null;
  computer: Computer | null;
  messages: Message[];
  files: Record<string, FileSummary>;
  // The Chat's approval cards: those still waiting and the ones answered here.
  approvals: Approval[];
  // A command is in flight; the composer waits for its answer.
  submitting: boolean;
  // What the Chat is doing, from the merged projection of its Runs. The
  // header, its marks, and the composer all read this one word, so a Run the
  // page does not follow still shows in the Chat.
  status: ChatStatus;
  // The followed Run waits; the Chat shows the place it holds.
  queue: RunProgress | null;
  // A Run of this Chat is active, whether or not this page follows it, so the
  // composer takes no second Message while the Bot works.
  busy: boolean;
  // The followed Run's tool activity, while it has any.
  activity: RunActivity | null;
  // The approval the User asked to see, so the Chat scrolls to its card.
  focus: string | null;
  // The Message a notification named, so the Chat scrolls to it.
  focusMessage: string | null;
  onFocused: () => void;
  // The approval whose answer is on its way to the Space.
  deciding: string | null;
  error: string | null;
  // Resolves false when nothing left the composer, which keeps its Message.
  onSend: (text: string, file: File | null) => Promise<boolean>;
  onDecide: (approval: Approval, allow: boolean, lasting: boolean) => void;
  onBack: () => void;
  // The reader left this Chat: the view is kept mounted, hidden by the page,
  // until every app in it answered the host's teardown request (or the page's
  // bounded wait ends). Its card rows close through the same `closing` path a
  // Message the Chat retired uses, so no frame is removed before it answers.
  retiring?: boolean;
  // The leaving view has no app row left to wait for, so the page may take it —
  // and the frames inside it — away.
  onRetired?: () => void;
}

const suggestions = [
  "chat.suggestion.hello",
  "chat.suggestion.work",
  "chat.suggestion.plan",
] as const;

// One row of the Chat's stream. The Message keeps the identity the reads gave
// it, so a refresh that answers with the same record keeps the row — and the
// open app's frame inside it — mounted. An app Card's row outlives its Message
// for the moment the app needs to answer the host's teardown request: the Chat
// cannot remove the frame's DOM without killing the document, so the row stays
// in place, hidden, until the app answers or the bounded wait ends.
interface ChatRow {
  message: Message;
  // Plain copies of what the row's bookkeeping needs. The effect phase must not
  // read a store proxy: a reactive read there is untracked and would never
  // update, and the Message object is the identity the row renders.
  id: string;
  // Whether the row holds an app: only that row has a document whose teardown
  // the Chat must let finish before the row leaves.
  app: boolean;
}

// One Message a snapshot lists, as the stream must show it.
interface ChatRowSource {
  message: Message;
  id: string;
  app: boolean;
}

// The rows the stream renders for a snapshot of Messages, reusing the row — and
// with it the AppCard and the app's document — by Message id. The snapshot's
// order always wins: a live event may show a later Message before a read
// supplies an earlier one, and once the authoritative read arrives the earlier
// Message belongs above the later one, not after it. A row whose Message is gone
// but whose app has not answered the host's teardown request keeps its place in
// the stream (or waits after the snapshot's rows), because its frame must stay
// attached to answer.
function orderRows(previous: ChatRow[], snapshot: ChatRowSource[]): ChatRow[] {
  const known = new Map(previous.map((row) => [row.id, row]));
  const listed = new Set(snapshot.map((entry) => entry.id));
  const rows = snapshot.map((entry) => {
    const row = known.get(entry.id);
    return row && row.message === entry.message && row.app === entry.app
      ? row
      : { message: entry.message, id: entry.id, app: entry.app };
  });
  const retired = previous.filter((row) => row.app && !listed.has(row.id));
  if (retired.length === 0) return rows;
  for (const row of retired) {
    const at = previous.indexOf(row);
    let anchor: string | null = null;
    for (let index = at + 1; index < previous.length; index += 1) {
      const candidate = previous[index];
      if (candidate && listed.has(candidate.id)) {
        anchor = candidate.id;
        break;
      }
    }
    const position = anchor === null ? -1 : rows.findIndex((entry) => entry.id === anchor);
    rows.splice(position === -1 ? rows.length : position, 0, row);
  }
  return rows;
}

export default function ChatView(props: ChatViewProps) {
  const [draft, setDraft] = createSignal("");
  const [attachment, setAttachment] = createSignal<File | null>(null);
  let stream: HTMLDivElement | undefined;
  let composer: HTMLTextAreaElement | undefined;
  let picker: HTMLInputElement | undefined;

  // The rows the stream renders. A row follows its Message object: a refresh
  // that answers with the same record keeps the object, so the row — and the
  // open app's frame inside it — is not remounted; a changed Message gets a new
  // row, as it did before. An app Card's row whose Message left the list stays
  // here until its app answers the host's teardown request (or the bounded wait
  // ends), hidden and in its place.
  const [rows, setRows] = createSignal<ChatRow[]>([]);
  // The Message ids the Chat holds now. A row whose id is gone is being retired:
  // its app is asked to tear down, and only its own answer lets the row leave.
  const present = createMemo(() => new Set(props.messages.map((message) => message.id)));
  const retiring = (row: ChatRow) => row.app && !present().has(row.id);

  createEffect(
    // The compute phase reads the store's list inside tracking and hands the
    // effect phase plain values, so the row bookkeeping never touches a proxy.
    () =>
      props.messages.map((message) => ({
        message,
        id: message.id,
        app: message.card?.kind === "app",
      })),
    (messages) => {
      // The snapshot's order is the stream's order, while a row that already
      // shows the same Message keeps its identity — and so does the app's frame
      // inside it.
      setRows((previous) => orderRows(previous, messages));
    },
  );

  // The app answered the teardown request (or the bounded wait ended), so its
  // hidden row may finally leave the stream.
  function removeRow(row: ChatRow) {
    setRows((previous) => previous.filter((entry) => entry !== row));
  }

  // A view the page is retiring tells the page once its last app closed: only
  // then may the view, and the frames inside it, go.
  createEffect(
    () => props.retiring === true && !rows().some((row) => row.app),
    (settled) => {
      if (settled) props.onRetired?.();
    },
  );

  createEffect(
    () => ({ count: props.messages.length, status: props.status }),
    () => {
      queueMicrotask(() => {
        if (stream) stream.scrollTop = stream.scrollHeight;
      });
    },
  );

  // An id comes from a URL, so compare it as a value instead of interpolating
  // it into a selector.
  function findAttribute(name: string, value: string): HTMLElement | undefined {
    if (!stream) return undefined;
    return Array.from(stream.querySelectorAll<HTMLElement>(`[${name}]`)).find(
      (element) => element.getAttribute(name) === value,
    );
  }

  // An approval the attention list named is the one the User came to read, so
  // the Chat brings its card into view once the record has delivered it. The
  // cards arrive with the Chat's reads, so the ask also waits for their count.
  createEffect(
    () => ({ approval: props.focus, count: props.approvals.length }),
    ({ approval }) => {
      if (!approval) return;
      const card = findAttribute("data-approval", approval);
      if (!card) return;
      queueMicrotask(() => {
        if (typeof card.scrollIntoView !== "function") return;
        card.scrollIntoView({ block: "center" });
        props.onFocused();
      });
    },
  );

  // A notification that named a Message opens the Chat at that Message.
  createEffect(
    () => ({ message: props.focusMessage, count: props.messages.length }),
    ({ message }) => {
      if (!message) return;
      // The rows render in the same turn the Message's arrival schedules this
      // effect, so the lookup waits for that render rather than reading the
      // stream before it holds the row.
      queueMicrotask(() => {
        const row = findAttribute("data-message", message);
        if (!row) return;
        if (typeof row.scrollIntoView !== "function") return;
        row.scrollIntoView({ block: "center" });
        props.onFocused();
      });
    },
  );

  const canSend = () =>
    !props.submitting &&
    !props.busy &&
    !!props.bot &&
    (draft().trim() !== "" || attachment() !== null);

  // How many requests a waiting Run has ahead. A running Run shows the Bot
  // working instead, so the two never show together.
  const queueLabel = (): string | null => {
    const queue = props.queue;
    if (queue?.status !== "queued") return null;
    // The running run holds place 1, so place N has N - 1 requests ahead.
    const ahead = Math.max((queue.position ?? 1) - 1, 0);
    if (ahead === 0) return t("chat.queue.queued");
    return t(ahead === 1 ? "chat.queue.waiting.one" : "chat.queue.waiting.many", { count: ahead });
  };

  async function send(event?: SubmitEvent) {
    event?.preventDefault();
    const text = draft().trim();
    const file = attachment();
    if (!canSend()) return;
    setDraft("");
    setAttachment(null);
    if (!(await props.onSend(text, file))) {
      setDraft(text);
      setAttachment(file);
    }
    if (composer) composer.focus();
  }

  function onKeyDown(event: KeyboardEvent) {
    if (event.isComposing || event.keyCode === 229) return;
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      void send();
    }
  }

  function pick(event: Event) {
    const input = event.currentTarget as HTMLInputElement;
    setAttachment(input.files?.[0] ?? null);
    // Clearing lets the same file be picked again after it is removed.
    input.value = "";
  }

  const title = () => props.bot?.name ?? t("chat.title");
  // What the Chat waits for: an approval, or the Bot's work.
  const waiting = () => props.status === "waiting";
  const failed = () => props.status === "failed";

  // Why the composer takes no Message: the Run waits in the queue, waits for an
  // approval, or works.
  const closedReason = () =>
    waiting()
      ? t("chat.composer.waiting", { name: title() })
      : t("chat.composer.busy", { name: title() });

  return (
    <section class="chat" aria-label={t("chat.aria", { name: title() })}>
      <header class="chat__head">
        <button class="chat__back" type="button" onClick={props.onBack} aria-label={t("chat.back")}>
          <Icon name="chevron-left" size="sm" />
        </button>
        <Avatar name={title()} />
        <div class="chat__titles">
          <h1>{title()}</h1>
          <p class="chat__sub">
            <Show when={props.computer} fallback={<span>{t("chat.loading")}</span>}>
              {(computer) => (
                <>
                  <span>
                    {computer().kind === "local"
                      ? t("chat.localComputer")
                      : t("chat.hostedComputer")}
                  </span>
                  <span class="chat__dot" />
                  <span class={computer().online ? "state-text state-text--online" : "state-text"}>
                    {computer().online ? t("chat.online") : t("chat.offline")}
                  </span>
                </>
              )}
            </Show>
          </p>
        </div>
        <Show when={waiting()}>
          <span
            class="chat__attention"
            role="status"
            aria-label={t("chat.waiting")}
            title={t("chat.waiting")}
          >
            <Icon name="alert" size="sm" />
            <span class="chat__attention-long">{t("chat.waiting")}</span>
            <span class="chat__attention-short">{t("chat.waiting.short")}</span>
          </span>
        </Show>
        <Show when={props.busy && !waiting() && !failed()}>
          <span
            class="chat__state"
            role="status"
            aria-label={t("chat.working")}
            title={t("chat.working")}
          >
            <Icon name="clock" size="sm" />
            <span class="chat__state-long">{t("chat.working")}</span>
          </span>
        </Show>
        <Show when={failed()}>
          <span
            class="chat__state chat__state--failed"
            role="status"
            aria-label={t("chat.failed")}
            title={t("chat.failed")}
          >
            <Icon name="alert" size="sm" />
            <span class="chat__state-long">{t("chat.failed")}</span>
          </span>
        </Show>
      </header>

      <Show when={props.error}>
        <div class="banner banner--error banner--inchat" role="alert">
          <Icon name="alert" size="sm" />
          {props.error}
        </div>
      </Show>

      <div class="chat__stream" ref={stream}>
        <Show
          // A retired Card's hidden row is still a row: the stream stays while
          // the app answers the host's teardown request.
          when={rows().length > 0 || props.approvals.length > 0}
          fallback={
            <div class="chat__empty">
              <Show when={props.bot}>
                {(bot) => (
                  <div class="chat__empty-avatar">
                    <Avatar name={bot().name} />
                  </div>
                )}
              </Show>
              <h2>
                {props.bot ? t("chat.new.title", { name: props.bot.name }) : t("chat.opening")}
              </h2>
              <p class="chat__empty-text">{t("chat.new.text")}</p>
              <div class="chips">
                <For each={suggestions}>
                  {(suggestion) => {
                    const text = () => t(suggestion);
                    return (
                      <button
                        class="chip chip--button"
                        type="button"
                        onClick={() => setDraft(text())}
                      >
                        {text()}
                      </button>
                    );
                  }}
                </For>
              </div>
            </div>
          }
        >
          <For each={rows()}>
            {(row, index) => {
              const message = row.message;
              const previous = () => rows()[index() - 1]?.message;
              const newDay = () =>
                !previous() ||
                dayLabel(previous()!.at, locale()) !== dayLabel(message.at, locale());
              const isBot = () => message.author.kind === "bot";
              // The row is kept attached, in its place, while its app answers
              // the teardown request; it is only hidden, never moved, because a
              // moved iframe resets the document the app is running in. A view
              // the page is retiring hides and closes every app row the same
              // way: the page keeps the frame alive until the app answers.
              const hidden = () => props.retiring === true || retiring(row);
              return (
                <div
                  class={hidden() ? "chat-row chat-row--closing" : "chat-row"}
                  aria-hidden={hidden() ? "true" : undefined}
                >
                  <Show when={newDay()}>
                    <div class="dayline">
                      <span>{dayLabel(message.at, locale())}</span>
                    </div>
                  </Show>
                  <article
                    class={`msg ${isBot() ? "msg--bot" : "msg--user"}`}
                    data-message={message.id}
                  >
                    <Show when={isBot()}>
                      <Avatar name={props.bot?.name ?? "Bot"} small />
                    </Show>
                    <div class="msg__body">
                      <div class="msg__meta">
                        <b>{isBot() ? (props.bot?.name ?? "Bot") : props.account.display_name}</b>
                        <time datetime={message.at}>{timeOfDay(message.at)}</time>
                        <Show when={isBot() && message.acting}>
                          <span>{t("chat.acting")}</span>
                        </Show>
                      </div>
                      <Show when={message.text}>
                        <p class="msg__text">{message.text}</p>
                      </Show>
                      <Show when={message.file}>
                        {(id) => <FileCard id={id()} file={props.files[id()] ?? null} />}
                      </Show>
                      {/* A Message shows its File and its Card side by side;
                          neither is nested in the other. */}
                      <Show when={message.card}>
                        {(card) => (
                          <Show
                            when={card().kind === "app"}
                            fallback={<CardPlaceholder kind={card().kind} />}
                          >
                            <AppCard
                              card={card()}
                              file={cardFile(props.files, card().file)}
                              onSend={(text) => props.onSend(text, null)}
                              closing={hidden()}
                              onClosed={() => removeRow(row)}
                            />
                          </Show>
                        )}
                      </Show>
                    </div>
                  </article>
                </div>
              );
            }}
          </For>

          <For each={props.approvals}>
            {(approval) => (
              <ApprovalCard
                approval={approval}
                botName={props.bot?.name ?? "Bot"}
                pending={props.deciding === approval.approval}
                onDecide={props.onDecide}
              />
            )}
          </For>

          <Show when={props.activity}>
            {(activity) => (
              <ActivityCard
                activity={activity()}
                botName={props.bot?.name ?? "Bot"}
                files={props.files}
                failed={failed()}
              />
            )}
          </Show>
        </Show>
      </div>

      <form class="composer" onSubmit={(event) => void send(event)}>
        <Show when={attachment()} keyed>
          {(file) => (
            <div class="attachment">
              <Icon name="file" size="sm" />
              <span class="attachment__name">{file.name}</span>
              <span class="attachment__size">{formatBytes(file.size)}</span>
              <button
                class="attachment__remove"
                type="button"
                aria-label={t("chat.attachment.remove", { name: file.name })}
                onClick={() => setAttachment(null)}
              >
                <Icon name="x" size="sm" />
              </button>
            </div>
          )}
        </Show>
        <Show when={props.busy}>
          <p class={queueLabel() ? "composer__queue" : "composer__closed"} role="status">
            {queueLabel() ?? closedReason()}
          </p>
        </Show>
        <textarea
          ref={composer}
          class="composer__input"
          name="message"
          rows={1}
          placeholder={
            props.bot
              ? t("chat.placeholder", { name: props.bot.name })
              : t("chat.placeholder.empty")
          }
          value={draft()}
          disabled={!props.bot || props.busy}
          onInput={(event) => setDraft(event.currentTarget.value)}
          onKeyDown={onKeyDown}
        />
        <div class="composer__foot">
          <div class="composer__tools">
            <button
              class="composer__attach"
              type="button"
              aria-label={t("chat.attach")}
              title={t("chat.attach")}
              disabled={!props.bot || props.submitting || props.busy}
              onClick={() => picker?.click()}
            >
              <Icon name="paperclip" size="sm" />
            </button>
            <span class="composer__hint">{t("chat.hint")}</span>
          </div>
          <button
            class="btn btn--primary btn--icon-only"
            type="submit"
            aria-label={t("chat.send")}
            disabled={!canSend()}
          >
            <Icon name="arrow-up" size="sm" />
          </button>
        </div>
        <input
          ref={picker}
          type="file"
          hidden
          disabled={!props.bot || props.submitting || props.busy}
          onChange={pick}
        />
      </form>
    </section>
  );
}
