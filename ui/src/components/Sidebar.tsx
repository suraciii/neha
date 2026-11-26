import { For, Show } from "solid-js";

import { t } from "../i18n";
import type { Account, Bot, Chat, Preview } from "../types";
import Avatar from "./Avatar";
import Icon from "./Icon";
import LocaleSwitch from "./LocaleSwitch";

interface SidebarProps {
  account: Account;
  chats: Chat[];
  bots: Bot[];
  activeChat: Chat | null;
  previews: Record<string, Preview>;
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

  return (
    <aside class="sidebar" aria-label={t("sidebar.chats")}>
      <header class="sidebar__head">
        <div>
          <h1 class="sidebar__title">{t("sidebar.chats")}</h1>
          <p class="sidebar__sub">
            {props.chats.length === 1
              ? t("sidebar.conversation.one")
              : t("sidebar.conversation.many", { count: props.chats.length })}
          </p>
        </div>
      </header>

      <div class="sidebar__list">
        <Show
          when={props.chats.length > 0}
          fallback={
            <div class="side-empty">
              <span class="side-empty__icon">
                <Icon name="message" size="lg" />
              </span>
              <p class="side-empty__title">{t("sidebar.empty.title")}</p>
              <p class="side-empty__text">{t("sidebar.empty.text")}</p>
            </div>
          }
        >
          <For each={props.chats}>
            {(chat) => {
              const bot = botFor(chat);
              return (
                <button
                  class="conv"
                  classList={{ "is-active": props.activeChat?.id === chat.id }}
                  type="button"
                  onClick={() => props.onSelect(chat)}
                >
                  <Avatar name={bot?.name ?? "Bot"} />
                  <span class="conv__text">
                    <span class="conv__top">
                      <span class="conv__name">{bot?.name ?? "Bot"}</span>
                      <span class="conv__meta">{t("chat.title")}</span>
                    </span>
                    <span class="conv__preview">{preview(chat)}</span>
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
