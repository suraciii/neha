import { For, Show } from "solid-js";

import type { Account, Bot, Chat } from "../types";
import Avatar from "./Avatar";
import Icon from "./Icon";

interface SidebarProps {
  account: Account;
  chats: Chat[];
  bots: Bot[];
  activeChat: Chat | null;
  previews: Record<string, string>;
  onSelect: (chat: Chat) => void;
}

export default function Sidebar(props: SidebarProps) {
  const botFor = (chat: Chat) => props.bots.find((bot) => bot.id === chat.bot);
  const preview = (chat: Chat) => props.previews[chat.id] ?? "Open Chat";

  return (
    <aside class="sidebar" aria-label="Chats">
      <header class="sidebar__head">
        <div>
          <h1 class="sidebar__title">Chats</h1>
          <p class="sidebar__sub">{props.chats.length === 1 ? "1 conversation" : `${props.chats.length} conversations`}</p>
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
              <p class="side-empty__title">No Chats yet</p>
              <p class="side-empty__text">Create a Bot and its Chat will appear here.</p>
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
                      <span class="conv__meta">Chat</span>
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
          Space online
        </span>
        <span class="foot__meta">{props.account.display_name}</span>
      </footer>
    </aside>
  );
}
