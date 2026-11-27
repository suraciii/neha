import { For, Show, createMemo } from "solid-js";

import { t } from "../i18n";
import type { Account, AttentionEntry, Bot, Chat, ChatStatus, Preview } from "../types";
import Avatar from "./Avatar";
import Brand from "./Brand";
import Icon from "./Icon";
import LocaleSwitch from "./LocaleSwitch";
import type { RailView } from "./Rail";

// What the Settings surface opens: the Providers, the Models, or the catalog.
export type SettingsSection = "providers" | "models" | "catalog";

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
  // Every approval that waits for the User, one entry per approval.
  attention: AttentionEntry[];
  // The rail selected the attention list, so it fills the sidebar.
  attentionOpen: boolean;
  // What the rail selected: the Chats, the waiting approvals, or a management
  // surface. A management surface fills the sidebar with its own list.
  view: RailView;
  // The Bot whose page is open, when one is.
  activeBot: string | null;
  // What the Settings surface shows.
  section: SettingsSection;
  onSelect: (chat: Chat) => void;
  onAttention: (entry: AttentionEntry) => void;
  onBot: (bot: Bot) => void;
  onSection: (section: SettingsSection) => void;
}

// One approval that waits: its Bot, what it asks for, and where to answer it.
function AttentionRow(props: { entry: AttentionEntry; onOpen: (entry: AttentionEntry) => void }) {
  return (
    <button class="attention__row" type="button" onClick={() => props.onOpen(props.entry)}>
      <Avatar name={props.entry.botName} small />
      <span class="attention__text">
        <b>{props.entry.botName}</b>
        <span>{props.entry.detail}</span>
      </span>
      <Icon name="chevron-right" size="sm" class="attention__go" />
    </button>
  );
}

// One entry of a management list: the Bot or the Settings section it opens.
function ManageRow(props: {
  icon: string;
  name: string;
  active: boolean;
  onSelect: () => void;
}) {
  return (
    <button
      class={`conv${props.active ? " is-active" : ""}`}
      type="button"
      aria-current={props.active ? "page" : undefined}
      onClick={props.onSelect}
    >
      <span class="conv__icon">
        <Icon name={props.icon} size="sm" />
      </span>
      <span class="conv__text">
        <span class="conv__top">
          <span class="conv__name">{props.name}</span>
        </span>
      </span>
    </button>
  );
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
  // How the count of waiting approvals reads.
  const countLabel = () =>
    props.attention.length === 1
      ? t("attention.count.one")
      : t("attention.count.many", { count: props.attention.length });

  // A management surface fills the sidebar with its own list.
  const managing = () => props.view === "bots" || props.view === "settings";
  const label = () =>
    props.attentionOpen
      ? t("attention.title")
      : props.view === "bots"
        ? t("sidebar.bots")
        : props.view === "settings"
          ? t("sidebar.settings")
          : t("sidebar.chats");

  const sections: { section: SettingsSection; icon: string }[] = [
    { section: "providers", icon: "server" },
    { section: "models", icon: "cpu" },
    { section: "catalog", icon: "book" },
  ];

  return (
    <aside class="sidebar" aria-label={label()}>
      <Show
        when={managing()}
        fallback={
          <>
            <Show
              when={!props.attentionOpen}
              fallback={
                <>
                  <header class="sidebar__head">
                    <Brand compact />
                    <div>
                      <h1 class="sidebar__title">{t("attention.title")}</h1>
                      <p class="sidebar__sub">{countLabel()}</p>
                    </div>
                  </header>
                  <div class="sidebar__list">
                    <Show
                      when={props.attention.length > 0}
                      fallback={
                        <div class="side-empty">
                          <p class="side-empty__title">{t("attention.empty.title")}</p>
                          <p class="side-empty__text">{t("attention.empty.text")}</p>
                        </div>
                      }
                    >
                      <For each={props.attention}>
                        {(entry) => <AttentionRow entry={entry} onOpen={props.onAttention} />}
                      </For>
                    </Show>
                  </div>
                </>
              }
            >
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

              {/* The same list the attention view shows, above the Chats, so an
                  approval stays one click away while the User reads. */}
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
                    {(entry) => <AttentionRow entry={entry} onOpen={props.onAttention} />}
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
            </Show>
          </>
        }
      >
        <header class="sidebar__head">
          <Brand compact />
          <div>
            <h1 class="sidebar__title">{props.view === "bots" ? t("sidebar.bots") : t("sidebar.settings")}</h1>
            <Show when={props.view === "bots"}>
              <p class="sidebar__sub">
                {props.bots.length === 1
                  ? t("sidebar.bots.one")
                  : t("sidebar.bots.many", { count: props.bots.length })}
              </p>
            </Show>
          </div>
        </header>

        <div class="sidebar__list">
          <Show
            when={props.view === "bots"}
            fallback={
              <For each={sections}>
                {(entry) => (
                  <ManageRow
                    icon={entry.icon}
                    name={t(
                      entry.section === "providers"
                        ? "sidebar.settings.providers"
                        : entry.section === "models"
                          ? "sidebar.settings.models"
                          : "sidebar.settings.catalog",
                    )}
                    active={props.section === entry.section}
                    onSelect={() => props.onSection(entry.section)}
                  />
                )}
              </For>
            }
          >
            <Show
              when={props.bots.length > 0}
              fallback={
                <div class="side-empty">
                  <p class="side-empty__title">{t("sidebar.bots.empty.title")}</p>
                  <p class="side-empty__text">{t("sidebar.bots.empty.text")}</p>
                </div>
              }
            >
              <For each={props.bots}>
                {(bot) => (
                  <ManageRow
                    icon="cpu"
                    name={bot.name}
                    active={props.activeBot === bot.id}
                    onSelect={() => props.onBot(bot)}
                  />
                )}
              </For>
            </Show>
          </Show>
        </div>
      </Show>

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
