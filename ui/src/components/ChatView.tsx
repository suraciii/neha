import { For, Show, createEffect, createSignal } from "solid-js";

import { locale, t } from "../i18n";
import { dayLabel, timeOfDay } from "../lib/format";
import { formatBytes } from "../lib/files";
import type { Account, Bot, Computer, FileSummary, Message } from "../types";
import Avatar from "./Avatar";
import FileCard from "./FileCard";
import Icon from "./Icon";
import LocaleSwitch from "./LocaleSwitch";

interface ChatViewProps {
  account: Account;
  bot: Bot | null;
  computer: Computer | null;
  messages: Message[];
  files: Record<string, FileSummary>;
  working: boolean;
  error: string | null;
  // Resolves false when nothing left the composer, which keeps its Message.
  onSend: (text: string, file: File | null) => Promise<boolean>;
  onBack: () => void;
}

const suggestions = ["chat.suggestion.hello", "chat.suggestion.work", "chat.suggestion.plan"] as const;

export default function ChatView(props: ChatViewProps) {
  const [draft, setDraft] = createSignal("");
  const [attachment, setAttachment] = createSignal<File | null>(null);
  let stream: HTMLDivElement | undefined;
  let composer: HTMLTextAreaElement | undefined;
  let picker: HTMLInputElement | undefined;

  createEffect(
    () => ({ count: props.messages.length, working: props.working }),
    () => {
      queueMicrotask(() => {
        if (stream) stream.scrollTop = stream.scrollHeight;
      });
    },
  );

  const canSend = () =>
    !props.working && !!props.bot && (draft().trim() !== "" || attachment() !== null);

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
            {props.bot ? t("chat.bot") : t("chat.loading")}
            <span class="chat__dot" />
            {props.computer?.kind === "managed" ? t("chat.managedComputer") : t("chat.computer")}
            <span class="chat__dot" />
            <span class={props.computer?.online ? "state-text state-text--online" : "state-text"}>
              {props.computer?.online ? t("chat.online") : t("chat.offline")}
            </span>
          </p>
        </div>
        <LocaleSwitch class="chat__locale" />
        <div class="chat__actions">
          <span class="chip">
            <Icon name="lock" size="sm" />
            {t("chat.ownerOnly")}
          </span>
        </div>
      </header>

      <Show when={props.error}>
        <div class="banner banner--error banner--inchat" role="alert">
          <Icon name="alert" size="sm" />
          {props.error}
        </div>
      </Show>

      <div class="chat__stream" ref={stream}>
        <Show
          when={props.messages.length > 0}
          fallback={
            <div class="chat__empty">
              <p class="eyebrow">{t("chat.new.eyebrow")}</p>
              <h2>
                {props.bot ? t("chat.new.title", { name: props.bot.name }) : t("chat.opening")}
              </h2>
              <p class="chat__empty-text">{t("chat.new.text")}</p>
              <div class="chips">
                <For each={suggestions}>
                  {(suggestion) => {
                    const text = () => t(suggestion);
                    return (
                      <button class="chip chip--button" type="button" onClick={() => setDraft(text())}>
                        {text()}
                      </button>
                    );
                  }}
                </For>
              </div>
            </div>
          }
        >
          <For each={props.messages}>
            {(message, index) => {
              const previous = () => props.messages[index() - 1];
              const newDay = () =>
                !previous() || dayLabel(previous()!.at, locale()) !== dayLabel(message.at, locale());
              const isBot = () => message.author.kind === "bot";
              return (
                <>
                  <Show when={newDay()}>
                    <div class="dayline">
                      <span>{dayLabel(message.at, locale())}</span>
                    </div>
                  </Show>
                  <article class={`msg ${isBot() ? "msg--bot" : "msg--user"}`}>
                    <Show when={isBot()}>
                      <Avatar name={props.bot?.name ?? "Bot"} small />
                    </Show>
                    <div class="msg__body">
                      <div class="msg__meta">
                        <b>{isBot() ? props.bot?.name ?? "Bot" : props.account.display_name}</b>
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
                    </div>
                  </article>
                </>
              );
            }}
          </For>

          <Show when={props.working}>
            <div class="msg msg--bot msg--working" aria-live="polite">
              <Avatar name={props.bot?.name ?? "Bot"} small />
              <div class="msg__body">
                <div class="msg__meta">
                  <b>{props.bot?.name ?? "Bot"}</b>
                  <span>{t("chat.working")}</span>
                </div>
                <div class="work-bars" aria-label={t("chat.working")}>
                  <i />
                  <i />
                  <i />
                </div>
              </div>
            </div>
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
        <textarea
          ref={composer}
          class="composer__input"
          name="message"
          rows={1}
          placeholder={
            props.bot ? t("chat.placeholder", { name: props.bot.name }) : t("chat.placeholder.empty")
          }
          value={draft()}
          disabled={!props.bot}
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
              disabled={!props.bot || props.working}
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
          class="composer__picker"
          type="file"
          aria-hidden="true"
          tabindex={-1}
          disabled={!props.bot || props.working}
          onChange={pick}
        />
      </form>
    </section>
  );
}
