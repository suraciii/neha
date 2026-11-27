import { For, Show, createMemo } from "solid-js";

import { t } from "../i18n";
import type { Account, AttentionEntry, Bot, Chat, ChatStatus, Preview } from "../types";
import Avatar from "./Avatar";
import Brand from "./Brand";
import Icon from "./Icon";
import LocaleSwitch from "./LocaleSwitch";

interface SidebarProps {
  account: Account;
  chats: Chat[];
  bots: Bot[];
  activeChat: Chat | null;
  previews: Record<string, Preview>;
  // What each Chat is doing right now, from the Space and the record's events.
  statuses: Record<string, ChatStatus>;
  // The Chats that received a Bot Message while another Chat was on screen.
  unread: Record<string, boolean>;
  // The Chats that wait for the User, in record order.
  attention: AttentionEntry[];
  onSelect: (chat: Chat) => void;
}

export default function Sidebar(props: SidebarProps) {
  const botFor = (chat: Chat) => props.bots.find((bot) => bot.id === chat.bot);
  const preview = (chat: Chat) => {
    const recorded = props.previews[chat.id];
    if (recorded && "text" in recorded) {
      if (recorded.text) return recorded.text;
      return recorded.file ? t("sidebar.file") : "";
    }
    if (recorded && "error" in recorded) return t("sidebar.openChat");
    const bot = botFor(chat);
    return bot ? t("sidebar.startChat", { name: bot.name }) : t("sidebar.newChat");
  };
  // How the count of waiting Chats reads.
  const countLabel = () =>
    props.attention.length === 1
      ? t("attention.count.one")
      : t("attention.count.many", { count: props.attention.length });

  return (
    <aside class="sidebar" aria-label={t("sidebar.chats")}>
      <header class="sidebar__head">
        <Brand compact />
        <div>
          <h1 class="sidebar__title">{t("sidebar.chats")}</h1>
          <p class="sidebar__sub">
            {props.chats.length === 1
              ? t("sidebar.chats.one")
              : t("sidebar.chats.many", { count: props.chats.length })}
          </p>
        </div>
      </header>

      <Show when={props.attention.length > 0}>
        <section class="attention" aria-label={t("attention.title")}>
          <header class="attention__head">
            <Icon name="bell" size="sm" />
            <h2 class="attention__title">{t("attention.title")}</h2>
            <span class="attention__count" title={countLabel()}>
              {props.attention.length}
            </span>
          </header>
          <For each={props.attention}>
            {(entry) => (
              <button
                class="attention__row"
                type="button"
                onClick={() => props.onSelect(entry.chat)}
              >
                <Avatar name={entry.botName} small />
                <span class="attention__text">
                  <b>{entry.botName}</b>
                  <span>{entry.detail ?? t("attention.waiting")}</span>
                </span>
                <Icon name="chevron-right" size="sm" class="attention__go" />
              </button>
            )}
          </For>
        </section>
      </Show>

      <div class="sidebar__list">
        <Show
          when={props.chats.length > 0}
          fallback={
            <div class="side-empty">
              <p class="side-empty__title">{t("sidebar.empty.title")}</p>
              <p class="side-empty__text">{t("sidebar.empty.text")}</p>
            </div>
          }
        >
          <For each={props.chats}>
            {(chat) => {
              const bot = createMemo(() => botFor(chat));
              const line = createMemo(() => preview(chat));
              // One mark per Chat: waiting comes first, then working, then the
              // activity a User has not opened yet.
              const mark = () => {
                const status = props.statuses[chat.id] ?? "idle";
                if (status === "waiting") return { kind: "waiting", label: t("chat.waiting.short") };
                if (status === "working") return { kind: "working", label: t("chat.working") };
                if (props.unread[chat.id]) return { kind: "new", label: t("chat.newActivity") };
                return null;
              };
              return (
                <button
                  class={`conv${props.activeChat?.id === chat.id ? " is-active" : ""}`}
                  type="button"
                  onClick={() => props.onSelect(chat)}
                >
                  <Avatar name={bot()?.name ?? "Bot"} />
                  <span class="conv__text">
                    <span class="conv__top">
                      <span class="conv__name">{bot()?.name ?? "Bot"}</span>
                      <Show when={mark()} fallback={<span class="conv__meta">{t("chat.title")}</span>}>
                        {(shown) => (
                          <span class={`conv__badge conv__badge--${shown().kind}`}>
                            <Show when={shown().kind === "waiting"} fallback={<i class="dot" />}>
                              <Icon name="alert" size="sm" />
                            </Show>
                            {shown().label}
                          </span>
                        )}
                      </Show>
                    </span>
                    <span class="conv__preview">{line()}</span>
                  </span>
                </button>
              );
            }}
          </For>
        </Show>
      </div>

      <footer class="sidebar__foot">
        <span class="foot__row">
          <span class="dot dot--online" />
          {t("sidebar.online")}
        </span>
        <div class="foot__row">
          <LocaleSwitch class="locale-switch--foot" />
          <span class="foot__meta">{props.account.display_name}</span>
        </div>
      </footer>
    </aside>
  );
}
