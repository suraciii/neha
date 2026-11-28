import { createMemo, createSignal, For, Show } from "solid-js";

import { computerLabel } from "../computer";
import { t } from "../i18n";
import type {
  Account,
  AttentionEntry,
  Bot,
  Chat,
  ChatStatus,
  Computer,
  PlanProgress,
  Preview,
} from "../types";
import Avatar from "./Avatar";
import Brand from "./Brand";
import Icon from "./Icon";
import LocaleSwitch from "./LocaleSwitch";
import NotificationSetting from "./NotificationSetting";
import type { RailView } from "./Rail";

// What the Settings surface opens: the Providers, the Models, the Computers, or
// the catalog.
export type SettingsSection = "providers" | "models" | "computers" | "devices" | "catalog";

interface SidebarProps {
  account: Account;
  chats: Chat[];
  bots: Bot[];
  activeChat: Chat | null;
  previews: Record<string, Preview>;
  // What each Chat is doing right now, from the Space and the record's events.
  statuses: Record<string, ChatStatus>;
  // How many entries of each working Chat's plan are done, when the page knows
  // the plan: the Working mark carries the count, so the row keeps one mark.
  progress: Record<string, PlanProgress>;
  // The Chats that received a Bot Message while another Chat was on screen.
  unread: Record<string, boolean>;
  // Every approval that waits for the User, one entry per approval.
  attention: AttentionEntry[];
  // The rail selected the attention list, so it fills the sidebar.
  attentionOpen: boolean;
  // What the rail selected: the Chats, the waiting approvals, a management
  // surface, or one of the libraries. A management surface fills the sidebar
  // with its own list; a library view marks its compact entry here.
  view: RailView;
  // The Bot whose page is open, when one is.
  activeBot: string | null;
  // What the Settings surface shows.
  section: SettingsSection;
  onSelect: (chat: Chat) => void;
  onAttention: (entry: AttentionEntry) => void;
  onBot: (bot: Bot) => void;
  onSection: (section: SettingsSection) => void;
  // A compact entry to a library view, one tap away from the Chat list.
  onView: (view: RailView) => void;
  // Creates a Bot under the name and the Computer the form holds, and answers
  // what the Space said when it made none.
  onCreateBot: (name: string, computer?: string) => Promise<string | null>;
  // The Computers a new Bot may work on, which the Bots surface reads.
  computers: Computer[];
  // Whether the Space has a Provider to work with: the client creates no Bot
  // before one exists.
  canCreateBot: boolean;
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
function ManageRow(props: { icon: string; name: string; active: boolean; onSelect: () => void }) {
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

// Naming a Bot, in the list it is about to join. The form owns what it is doing:
// an empty name sends nothing, a refusal is the form's own to say, and a create
// that is on its way cannot be taken back.
function NewBotForm(props: {
  computers: Computer[];
  onCreate: (name: string, computer?: string) => Promise<string | null>;
  onCancel: () => void;
}) {
  const [name, setName] = createSignal("");
  // A new Bot works on the owner's Space Computer unless the User gives it one
  // they already paired.
  const [computer, setComputer] = createSignal("");
  const [sending, setSending] = createSignal(false);
  const [refused, setRefused] = createSignal<string | null>(null);
  // The Local Computers the User owns, which are the ones a new Bot may be
  // given. One that is offline is still theirs, so it is offered and explained
  // rather than left out, and it cannot be chosen while it is.
  const locals = () => props.computers.filter((known) => known.kind === "local");
  const blank = () => name().trim() === "";

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (sending() || blank()) return;
    const chosen = computer();
    setSending(true);
    setRefused(null);
    const said = await props.onCreate(name().trim(), chosen === "" ? undefined : chosen);
    // A create the Space answered is done: the Bot is there, so the form closes
    // over a clean slate whatever the lists went on to say.
    setSending(false);
    if (said === null) {
      setName("");
      setComputer("");
      props.onCancel();
      return;
    }
    // A refusal keeps the name the User typed, and says what the Space said, so
    // the form is the one place to fix it.
    setRefused(said);
  }

  return (
    <form class="side-form" onSubmit={submit}>
      <label class="field">
        <span class="field__label">{t("setup.bot.name")}</span>
        <input
          class="input"
          name="bot_name"
          type="text"
          value={name()}
          placeholder={t("setup.bot.nameHint")}
          onInput={(event) => setName(event.currentTarget.value)}
        />
      </label>
      <Show when={locals().length > 0}>
        <label class="field">
          <span class="field__label">{t("bots.new.computer")}</span>
          <select
            class="input"
            name="bot_computer"
            value={computer()}
            onChange={(event) => setComputer(event.currentTarget.value)}
          >
            <option value="">{t("computer.space")}</option>
            <For each={locals()}>
              {(known) => (
                <option value={known.id} disabled={!known.online}>
                  {computerLabel(known)}
                </option>
              )}
            </For>
          </select>
        </label>
        <Show when={locals().some((known) => !known.online)}>
          <span class="field__hint">{t("bots.new.computer.offline")}</span>
        </Show>
      </Show>
      <div class="side-form__actions">
        <button class="btn btn--primary" type="submit" disabled={sending() || blank()}>
          {t("setup.bot.create")}
        </button>
        <button
          class="btn btn--quiet"
          type="button"
          disabled={sending()}
          onClick={() => props.onCancel()}
        >
          {t("common.cancel")}
        </button>
      </div>
      <Show when={refused()}>
        <p class="side-form__error" role="alert">
          {refused()}
        </p>
      </Show>
    </form>
  );
}

export default function Sidebar(props: SidebarProps) {
  const botFor = (chat: Chat) => props.bots.find((bot) => bot.id === chat.bot);
  // Naming a new Bot, which the Bots list holds the action for. The form
  // replaces the list rather than sitting under it: a list that long scrolls
  // away from the name being typed.
  const [naming, setNaming] = createSignal(false);
  const preview = (chat: Chat) => {
    const recorded = props.previews[chat.id];
    if (recorded && "text" in recorded) {
      if (recorded.text) return recorded.text;
      if (recorded.card) return t("sidebar.app");
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
    { section: "computers", icon: "monitor" },
    { section: "devices", icon: "key" },
    { section: "catalog", icon: "book" },
  ];

  return (
    <aside class="sidebar" aria-label={label()}>
      {/* The library views the rail opens stay one tap away from the Chat
          list, and their compact entries mark the open one. */}
      <nav class="sidebar__views" aria-label={t("rail.primary")}>
        <button
          class={`chip chip--button${props.view === "integrations" ? " is-active" : ""}`}
          type="button"
          aria-current={props.view === "integrations" ? "page" : undefined}
          onClick={() => props.onView("integrations")}
        >
          <Icon name="server" size="sm" />
          {t("rail.integrations")}
        </button>
        <button
          class={`chip chip--button${props.view === "skills" ? " is-active" : ""}`}
          type="button"
          aria-current={props.view === "skills" ? "page" : undefined}
          onClick={() => props.onView("skills")}
        >
          <Icon name="book" size="sm" />
          {t("rail.skills")}
        </button>
      </nav>
      <Show
        when={managing()}
        fallback={
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
                    // activity a User has not opened yet. A working Chat whose plan
                    // the page knows names its done count in the same mark.
                    const mark = () => {
                      const status = props.statuses[chat.id] ?? "idle";
                      if (status === "waiting")
                        return { kind: "waiting", label: t("chat.waiting.short") };
                      if (status === "working") {
                        const progress = props.progress[chat.id];
                        const label = progress
                          ? t("chat.working.plan", {
                              done: progress.done,
                              total: progress.total,
                            })
                          : t("chat.working");
                        return { kind: "working", label };
                      }
                      if (props.unread[chat.id])
                        return { kind: "new", label: t("chat.newActivity") };
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
                            <Show
                              when={mark()}
                              fallback={<span class="conv__meta">{t("chat.title")}</span>}
                            >
                              {(shown) => (
                                <span class={`conv__badge conv__badge--${shown().kind}`}>
                                  <Show
                                    when={shown().kind === "waiting"}
                                    fallback={<i class="dot" />}
                                  >
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
        }
      >
        <header class="sidebar__head">
          <Brand compact />
          <div>
            <h1 class="sidebar__title">
              {props.view === "bots" ? t("sidebar.bots") : t("sidebar.settings")}
            </h1>
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
                          : entry.section === "computers"
                            ? "sidebar.settings.computers"
                            : entry.section === "devices"
                              ? "sidebar.settings.devices"
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
              when={!naming()}
              fallback={
                <NewBotForm
                  computers={props.computers}
                  onCreate={(name, computer) => props.onCreateBot(name, computer)}
                  onCancel={() => setNaming(false)}
                />
              }
            >
              <Show when={props.canCreateBot}>
                <button class="side-new" type="button" onClick={() => setNaming(true)}>
                  <Icon name="plus" size="sm" />
                  {t("bots.new")}
                </button>
              </Show>
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
          <NotificationSetting />
          <span class="foot__meta">{props.account.display_name}</span>
        </div>
      </footer>
    </aside>
  );
}
