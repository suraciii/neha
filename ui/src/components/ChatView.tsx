import { For, Show, createEffect, createSignal } from "solid-js";

import { dayLabel, timeOfDay } from "../lib/format";
import type { Account, Bot, Computer, Message } from "../types";
import Avatar from "./Avatar";
import Icon from "./Icon";

interface ChatViewProps {
  account: Account;
  bot: Bot | null;
  computer: Computer | null;
  messages: Message[];
  working: boolean;
  error: string | null;
  onSend: (text: string) => void;
  onBack: () => void;
}

const suggestions = ["Say hello", "What are you working on?", "Help me plan the next step."];

export default function ChatView(props: ChatViewProps) {
  const [draft, setDraft] = createSignal("");
  let stream: HTMLDivElement | undefined;
  let composer: HTMLTextAreaElement | undefined;

  createEffect(() => {
    props.messages.length;
    props.working;
    queueMicrotask(() => {
      if (stream) stream.scrollTop = stream.scrollHeight;
    });
  });

  function send(event?: SubmitEvent) {
    event?.preventDefault();
    const text = draft().trim();
    if (!text || props.working) return;
    setDraft("");
    props.onSend(text);
    if (composer) composer.focus();
  }

  function onKeyDown(event: KeyboardEvent) {
    if (event.isComposing || event.keyCode === 229) return;
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      send();
    }
  }

  const title = () => props.bot?.name ?? "Chat";

  return (
    <section class="chat" aria-label={`Chat with ${title()}`}>
      <header class="chat__head">
        <button class="chat__back" type="button" onClick={props.onBack} aria-label="Back to Chats">
          <Icon name="chevron-left" size="sm" />
        </button>
        <Avatar name={title()} />
        <div class="chat__titles">
          <h1>{title()}</h1>
          <p class="chat__sub">
            {props.bot ? "Bot" : "Loading…"}
            <span class="chat__dot" />
            {props.computer?.kind === "managed" ? "Managed Computer" : "Computer"}
            <span class="chat__dot" />
            <span class={props.computer?.online ? "state-text state-text--online" : "state-text"}>
              {props.computer?.online ? "Online" : "Offline"}
            </span>
          </p>
        </div>
        <div class="chat__actions">
          <span class="chip">
            <Icon name="lock" size="sm" />
            Owner only
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
              <p class="eyebrow">A new Chat</p>
              <h2>{props.bot ? `Say hello to ${props.bot.name}.` : "Opening the Chat…"}</h2>
              <p class="chat__empty-text">
                Give the Bot work in your own words. It answers here, and the record keeps the thread.
              </p>
              <div class="chips">
                <For each={suggestions}>
                  {(suggestion) => (
                    <button class="chip chip--button" type="button" onClick={() => setDraft(suggestion)}>
                      {suggestion}
                    </button>
                  )}
                </For>
              </div>
            </div>
          }
        >
          <For each={props.messages}>
            {(message, index) => {
              const previous = () => props.messages[index() - 1];
              const newDay = () => !previous() || dayLabel(previous()!.at) !== dayLabel(message.at);
              const isBot = () => message.author.kind === "bot";
              return (
                <>
                  <Show when={newDay()}>
                    <div class="dayline">
                      <span>{dayLabel(message.at)}</span>
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
                          <span>Acting for you</span>
                        </Show>
                      </div>
                      <p class="msg__text">{message.text}</p>
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
                  <span>Working</span>
                </div>
                <div class="work-bars" aria-label="Working">
                  <i />
                  <i />
                  <i />
                </div>
              </div>
            </div>
          </Show>
        </Show>
      </div>

      <form class="composer" onSubmit={(event) => send(event)}>
        <textarea
          ref={composer}
          class="composer__input"
          name="message"
          rows={1}
          placeholder={props.bot ? `Message ${props.bot.name}…` : "Message the Bot…"}
          value={draft()}
          disabled={!props.bot}
          onInput={(event) => setDraft(event.currentTarget.value)}
          onKeyDown={onKeyDown}
        />
        <div class="composer__foot">
          <span class="composer__hint">Enter to send · Shift+Enter for a new line</span>
          <button
            class="btn btn--primary btn--icon-only"
            type="submit"
            aria-label="Send Message"
            disabled={props.working || draft().trim() === "" || !props.bot}
          >
            <Icon name="arrow-up" size="sm" />
          </button>
        </div>
      </form>
    </section>
  );
}
