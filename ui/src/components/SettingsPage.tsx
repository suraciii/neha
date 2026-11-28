import { For, Show, createSignal } from "solid-js";

import { locale, t, type MessageKey } from "../i18n";
import { dayLabel } from "../lib/format";
import type {
  Account,
  Bot,
  CatalogStatus,
  CatalogVendor,
  Computer,
  Model,
  Provider,
  ThinkingLevel,
} from "../types";
import Icon from "./Icon";
import type { SettingsSection } from "./Sidebar";

// The protocols a connection may name under Advanced.
const PROTOCOLS = ["chat_completions", "openai_responses", "anthropic_messages"] as const;

// What each protocol is called in the Admin's words.
const PROTOCOL_LABEL: Record<string, MessageKey> = {
  chat_completions: "settings.protocol.chat_completions",
  openai_responses: "settings.protocol.openai_responses",
  anthropic_messages: "settings.protocol.anthropic_messages",
};

// How much thought a Model can give, cheapest first.
const LEVELS: ThinkingLevel[] = ["minimal", "low", "medium", "high", "max"];

interface SettingsPageProps {
  account: Account;
  providers: Provider[];
  models: Model[];
  // Every Computer this Space can use, which is what the Computers section lists.
  computers: Computer[];
  // Every Bot, so a Computer says which Bots work on it.
  bots: Bot[];
  // Every vendor the catalog knows that this Space can connect.
  vendors: CatalogVendor[];
  catalog: CatalogStatus | null;
  section: SettingsSection;
  // A write is on its way to the Space.
  busy: boolean;
  error: string | null;
  onBack: () => void;
  // Creates a Hosted Computer with the name and sharing the User chooses.
  onCreate: (input: { name: string; sharing: Computer["sharing"] }) => Promise<boolean>;
  // Makes an exclusive Hosted Computer shared, so another Bot may use it.
  onShare: (computer: string) => Promise<boolean>;
  // Clears a Hosted Computer's Workspace, once the Admin confirms it.
  onReset: (computer: string) => Promise<boolean>;
  // Opens one Bot's page from a Computer that has Bots on it.
  onOpenBot: (bot: Bot) => void;
  onConnect: (input: {
    vendor: string;
    apiKey: string;
    baseUrl: string | null;
    protocol: string | null;
    thinking: ThinkingLevel | null;
  }) => Promise<boolean>;
  onRefresh: (provider: string) => Promise<boolean>;
  onDefault: (model: string) => Promise<boolean>;
  onCatalog: () => Promise<boolean>;
}

// Where a Model comes from, which is the base URL of its Provider.
function baseUrlOf(providers: Provider[], model: Model): string {
  return providers.find((known) => known.id === model.provider)?.base_url ?? "";
}

// What a Computer is called in the User's words.
function kindOf(computer: Computer): string {
  return computer.kind === "local" ? t("chat.localComputer") : t("chat.hostedComputer");
}

// Who may use a Computer, in the User's words.
function sharingOf(computer: Computer): string {
  return computer.sharing === "exclusive"
    ? t("computer.sharing.exclusive")
    : t("computer.sharing.shared");
}

// One Model, with what it can do and the offer to make it the default. A Model
// that cannot call tools cannot answer a Chat, so the Space refuses it as the
// default, and the page says so rather than offering a change that fails.
function ModelRow(props: {
  model: Model;
  baseUrl: string;
  admin: boolean;
  busy: boolean;
  onDefault: (model: string) => void;
}) {
  const model = () => props.model;
  return (
    <div class="row">
      <span class="row__text">
        <span class="row__name mono">{model().name}</span>
        <span class="row__meta">{props.baseUrl}</span>
        <span class="chips">
          <span class="chip">{t("bot.model.context", { limit: model().context_limit })}</span>
          <span class="chip">{t("bot.model.output", { limit: model().max_output })}</span>
          <span class="chip">
            {model().reads_images ? t("bot.model.readsImages") : t("bot.model.noImages")}
          </span>
          <span class="chip">
            {model().tools ? t("bot.model.callsTools") : t("bot.model.noTools")}
          </span>
          <Show when={model().is_default}>
            <span class="chip chip--default">{t("settings.models.default")}</span>
          </Show>
        </span>
      </span>
      <Show when={props.admin && !model().is_default}>
        <button
          class="btn btn--quiet"
          type="button"
          disabled={props.busy || !model().tools}
          title={model().tools ? undefined : t("settings.models.setDefault.noTools")}
          onClick={() => props.onDefault(model().id)}
        >
          {t("settings.models.setDefault")}
        </button>
      </Show>
    </div>
  );
}

// The protocol a connection names, in the Admin's words.
function protocolLabel(protocol: string): string {
  const key = PROTOCOL_LABEL[protocol];
  return key === undefined ? protocol : t(key);
}

// When the cache was last written, in the reader's language.
function stamp(seconds: number | null): string {
  if (seconds === null) return t("settings.catalog.never");
  return dayLabel(new Date(seconds * 1_000).toISOString(), locale());
}

export default function SettingsPage(props: SettingsPageProps) {
  const [vendor, setVendor] = createSignal("");
  const [apiKey, setApiKey] = createSignal("");
  const [baseUrl, setBaseUrl] = createSignal("");
  // An empty choice keeps the catalog's preset: an override travels only when
  // the Admin opens Advanced and fills it.
  const [protocol, setProtocol] = createSignal<string>("");
  const [thinking, setThinking] = createSignal<string>("");
  const [advanced, setAdvanced] = createSignal(false);
  // What the last accepted change did, so a quiet write still answers.
  const [note, setNote] = createSignal<string | null>(null);

  const canConnect = () =>
    props.account.is_admin && !props.busy && vendor() !== "" && apiKey() !== "";
  const connectable = () => props.vendors.filter((known) => known.selectable);
  // The Models each Provider offers, in the Providers' own order, so a Provider
  // that offers none still says so where its Models would be. A Model whose
  // Provider the list does not name keeps its own place, so no Model is lost.
  const modelGroups = () => {
    const named = new Set(props.providers.map((known) => known.id));
    const groups = props.providers.map((provider) => ({
      label: provider.base_url,
      models: props.models.filter((model) => model.provider === provider.id),
    }));
    const rest = props.models.filter((model) => !named.has(model.provider));
    return rest.length === 0 ? groups : [...groups, { label: "", models: rest }];
  };

  async function connect() {
    if (!canConnect()) return;
    setNote(null);
    // An override travels only when the Admin opened Advanced and filled it.
    // The catalog's own preset stands otherwise, so a key never reaches a base
    // URL the Admin did not ask for.
    const done = await props.onConnect({
      vendor: vendor(),
      apiKey: apiKey(),
      baseUrl: advanced() && baseUrl().trim() !== "" ? baseUrl().trim() : null,
      protocol: advanced() && protocol() !== "" ? protocol() : null,
      thinking: advanced() && thinking() !== "" ? (thinking() as ThinkingLevel) : null,
    });
    // The key is never shown again, so the form starts over either way.
    setApiKey("");
    setBaseUrl("");
    if (done) setNote(t("settings.providers.refreshed"));
  }

  async function refresh(provider: string) {
    if (props.busy) return;
    setNote(null);
    if (await props.onRefresh(provider)) setNote(t("settings.providers.refreshed"));
  }

  async function makeDefault(model: string) {
    if (props.busy) return;
    setNote(null);
    if (await props.onDefault(model)) setNote(t("settings.models.setDefault.done"));
  }

  async function catalog() {
    if (props.busy) return;
    setNote(null);
    if (await props.onCatalog()) setNote(t("settings.catalog.updated"));
  }

  const title = () =>
    props.section === "providers"
      ? t("sidebar.settings.providers")
      : props.section === "models"
        ? t("sidebar.settings.models")
        : props.section === "computers"
          ? t("sidebar.settings.computers")
          : t("sidebar.settings.catalog");

  // The Bots that work on one Computer, and what each of them may write. A
  // Local Computer's access belongs to its Bots, so this is where the summary
  // lives; the Bot's own page is where it changes.
  const botsOn = (computer: Computer) => props.bots.filter((bot) => bot.computer === computer.id);
  // A Computer whose daemon serves no fence runs the Bot's shell with the
  // machine user's own rights: the folders the Bot names are ones the daemon
  // would have to honor, and this one honors none of them. So the summary says
  // what the shell really runs as instead of a fence that is not there. A
  // Computer that has not answered says nothing about its machine yet, so the
  // summary asks for its word rather than guessing either way.
  const accessOf = (bot: Bot, computer: Computer | undefined): string => {
    if (computer && computer.sandboxed === false) {
      return t("settings.computers.access.machine");
    }
    if (computer && computer.sandboxed == null) {
      return t("settings.computers.access.unknown");
    }
    return bot.full_access
      ? t("settings.computers.access.full")
      : bot.grants.length === 0
        ? t("settings.computers.access.none")
        : t("settings.computers.access.folders", { count: bot.grants.length });
  };

  // The Computer whose reset the Admin armed with a first press. Clearing a
  // Workspace deletes its files, so it asks twice wherever it is asked from.
  const [armedReset, setArmedReset] = createSignal<string | null>(null);
  // The Hosted Computer the User is naming, and who may use it. It starts
  // exclusive, like the CLI's default, and can be made shared in the list.
  const [computerName, setComputerName] = createSignal("");
  const [computerSharing, setComputerSharing] =
    createSignal<Computer["sharing"]>("exclusive");

  async function createComputer() {
    const name = computerName().trim();
    if (name === "" || props.busy) return;
    setNote(null);
    if (await props.onCreate({ name, sharing: computerSharing() })) {
      setComputerName("");
      setComputerSharing("exclusive");
      setNote(t("settings.computers.create.done"));
    }
  }

  // An exclusive Computer refuses a second Bot, so making it shared is how its
  // owner lets another one use it.
  async function shareComputer(computer: string) {
    if (props.busy) return;
    setNote(null);
    if (await props.onShare(computer)) setNote(t("settings.computers.share.done"));
  }

  async function reset(computer: string) {
    if (props.busy) return;
    if (armedReset() !== computer) {
      setArmedReset(computer);
      return;
    }
    setArmedReset(null);
    if (await props.onReset(computer)) setNote(t("settings.computers.reset.done"));
  }

  return (
    <section class="page" aria-label={t("rail.settings")}>
      <header class="page__head">
        <button class="page__back" type="button" onClick={props.onBack} aria-label={t("sidebar.settings")}>
          <Icon name="chevron-left" size="sm" />
        </button>
        <div class="page__titles">
          <h1>{t("sidebar.settings")}</h1>
          <p class="page__sub">{title()}</p>
        </div>
      </header>

      <Show when={props.error}>
        <div class="banner banner--error page__error" role="alert">
          <Icon name="alert" size="sm" />
          {props.error}
        </div>
      </Show>
      <Show when={note()}>
        <p class="page__note" role="status">
          {note()}
        </p>
      </Show>

      <div class="page__body">
        <Show when={props.section === "providers"}>
          <section class="panel">
            <div class="panel__head">
              <div>
                <h2>{t("sidebar.settings.providers")}</h2>
              </div>
              <span class="badge">{props.providers.length}</span>
            </div>
            <div class="panel__body">
              <Show
                when={props.providers.length > 0}
                fallback={
                  <div class="side-empty">
                    <p class="side-empty__title">{t("settings.providers.empty.title")}</p>
                    <p class="side-empty__text">{t("settings.providers.empty.text")}</p>
                  </div>
                }
              >
                <div class="rows">
                  <For each={props.providers}>
                    {(provider) => (
                      <div class="row">
                        <span class="row__text">
                          <span class="row__name mono">{provider.base_url}</span>
                          <span class="row__meta">
                            {provider.vendor
                              ? `${provider.vendor} · ${protocolLabel(provider.protocol)}`
                              : protocolLabel(provider.protocol)}
                          </span>
                        </span>
                        <Show when={props.account.is_admin}>
                          <button
                            class="btn btn--quiet"
                            type="button"
                            disabled={props.busy}
                            onClick={() => void refresh(provider.id)}
                          >
                            <Icon name="repeat" size="sm" />
                            {props.busy ? t("settings.providers.refreshing") : t("settings.providers.refresh")}
                          </button>
                        </Show>
                      </div>
                    )}
                  </For>
                </div>
              </Show>

              <Show
                when={props.account.is_admin}
                fallback={<p class="panel__note">{t("settings.providers.adminOnly")}</p>}
              >
                <form
                  class="panel__form"
                  onSubmit={(event) => {
                    event.preventDefault();
                    void connect();
                  }}
                >
                  <label class="field">
                    <span class="field__label">{t("settings.providers.vendor")}</span>
                    <select
                      class="input"
                      name="vendor"
                      value={vendor()}
                      onChange={(event) => {
                        setVendor(event.currentTarget.value);
                        // A key, a base URL, and an override belong to the vendor
                        // they were given for.
                        setApiKey("");
                        setBaseUrl("");
                        setProtocol("");
                        setThinking("");
                      }}
                    >
                      <option value="">{t("settings.providers.connect")}</option>
                      <For each={connectable()}>
                        {(known) => <option value={known.id}>{known.name}</option>}
                      </For>
                    </select>
                    <span class="field__hint">
                      {connectable().length === 0 ? t("settings.providers.vendor.empty") : t("setup.provider.lede")}
                    </span>
                  </label>
                  <label class="field">
                    <span class="field__label">{t("setup.apiKey")}</span>
                    <input
                      class="input"
                      name="api_key"
                      type="password"
                      autocomplete="off"
                      value={apiKey()}
                      onInput={(event) => setApiKey(event.currentTarget.value)}
                    />
                    <span class="field__hint">{t("setup.apiKey.hint")}</span>
                  </label>

                  <button
                    class="link-btn"
                    type="button"
                    aria-expanded={advanced() ? "true" : "false"}
                    onClick={() => setAdvanced((open) => !open)}
                  >
                    <Icon name={advanced() ? "chevron-up" : "chevron-down"} size="sm" />
                    {t("setup.advanced")}
                  </button>
                  <Show when={advanced()}>
                    <div class="field-row">
                      <label class="field">
                        <span class="field__label">{t("settings.providers.baseUrl")}</span>
                        <input
                          class="input mono"
                          name="connect_base_url"
                          inputmode="url"
                          placeholder={props.vendors.find((known) => known.id === vendor())?.api ?? ""}
                          value={baseUrl()}
                          onInput={(event) => setBaseUrl(event.currentTarget.value)}
                        />
                      </label>
                      <label class="field">
                        <span class="field__label">{t("settings.providers.protocol")}</span>
                        <select
                          class="input"
                          name="connect_protocol"
                          value={protocol()}
                          onChange={(event) => setProtocol(event.currentTarget.value)}
                        >
                          <option value="">{t("settings.providers.protocol.default")}</option>
                          <For each={PROTOCOLS}>
                            {(known) => <option value={known}>{protocolLabel(known)}</option>}
                          </For>
                        </select>
                      </label>
                    </div>
                    <label class="field">
                      <span class="field__label">{t("settings.providers.thinking")}</span>
                      <select
                        class="input"
                        name="connect_thinking"
                        value={thinking()}
                        onChange={(event) => setThinking(event.currentTarget.value)}
                      >
                        <option value="">{t("bot.model.default")}</option>
                        <For each={LEVELS}>
                          {(level) => <option value={level}>{t(`level.${level}`)}</option>}
                        </For>
                      </select>
                    </label>
                  </Show>

                  <div class="panel__foot panel__foot--end">
                    <button class="btn btn--primary" type="submit" disabled={!canConnect()}>
                      {props.busy ? t("setup.connecting") : t("setup.connect")}
                      <Icon name="arrow-right" size="sm" />
                    </button>
                  </div>
                  <span class="field__hint">{t("setup.provider.note")}</span>
                </form>
              </Show>
            </div>
          </section>
        </Show>

        <Show when={props.section === "models"}>
          <section class="panel">
            <div class="panel__head">
              <div>
                <h2>{t("sidebar.settings.models")}</h2>
              </div>
              <span class="badge">{props.models.length}</span>
            </div>
            <div class="panel__body">
              <p class="panel__note">{t("settings.models.hint")}</p>
              <Show
                when={modelGroups().length > 0}
                fallback={
                  <div class="side-empty">
                    <p class="side-empty__title">{t("settings.models.empty.title")}</p>
                    <p class="side-empty__text">{t("settings.models.empty.text")}</p>
                  </div>
                }
              >
                <div class="rows">
                  <For each={modelGroups()}>
                    {(group) => (
                      <>
                        <For each={group.models}>
                          {(model) => (
                            <ModelRow
                              model={model}
                              baseUrl={baseUrlOf(props.providers, model)}
                              admin={props.account.is_admin}
                              busy={props.busy}
                              onDefault={(id) => void makeDefault(id)}
                            />
                          )}
                        </For>
                        <Show when={group.models.length === 0}>
                          <div class="row">
                            <span class="row__text">
                              <span class="row__name mono">{group.label}</span>
                              <span class="row__meta">{t("settings.models.empty.provider")}</span>
                            </span>
                          </div>
                        </Show>
                      </>
                    )}
                  </For>
                </div>
              </Show>
            </div>
          </section>
        </Show>

        <Show when={props.section === "catalog"}>
          <section class="panel">
            <div class="panel__head">
              <div>
                <h2>{t("sidebar.settings.catalog")}</h2>
              </div>
              <Show when={props.account.is_admin}>
                <button
                  class="btn btn--quiet"
                  type="button"
                  disabled={props.busy}
                  onClick={() => void catalog()}
                >
                  <Icon name="repeat" size="sm" />
                  {props.busy ? t("settings.catalog.refreshing") : t("settings.catalog.refresh")}
                </button>
              </Show>
            </div>
            <div class="panel__body">
              <p class="panel__note">{t("settings.catalog.hint")}</p>
              <Show
                when={props.catalog}
                fallback={<p class="rows__empty">{t("common.loading")}</p>}
              >
                {(catalog) => (
                  <div class="facts">
                    <div class="fact">
                      <span class="fact__label">{t("settings.catalog.source")}</span>
                      <span class="fact__value mono">
                        {catalog().builtin ? t("settings.catalog.builtin") : catalog().source}
                      </span>
                    </div>
                    <div class="fact">
                      <span class="fact__label">{t("settings.catalog.fetchedAt")}</span>
                      <span class="fact__value">{stamp(catalog().fetched_at)}</span>
                    </div>
                    <div class="fact">
                      <span class="fact__label">{t("settings.catalog.checkedAt")}</span>
                      <span class="fact__value">{stamp(catalog().checked_at)}</span>
                    </div>
                    <div class="fact">
                      <span class="fact__label">{t("settings.catalog.vendorCount")}</span>
                      <span class="fact__value">{catalog().provider_count}</span>
                    </div>
                    <div class="fact">
                      <span class="fact__label">{t("settings.catalog.modelCount")}</span>
                      <span class="fact__value">{catalog().model_count}</span>
                    </div>
                    <div class="fact">
                      <span class="fact__label">{t("settings.catalog.freshness")}</span>
                      <span class="fact__value">
                        {catalog().stale ? t("settings.catalog.stale") : t("settings.catalog.fresh")}
                      </span>
                    </div>
                  </div>
                )}
              </Show>
              <Show when={props.catalog?.builtin}>
                <p class="panel__note">{t("settings.catalog.kept")}</p>
              </Show>
            </div>
          </section>
        </Show>

        <Show when={props.section === "computers"}>
          <section class="panel">
            <div class="panel__head">
              <div>
                <h2>{t("sidebar.settings.computers")}</h2>
              </div>
              <span class="badge">{props.computers.length}</span>
            </div>
            <div class="panel__body">
              <p class="panel__note">{t("settings.computers.hint")}</p>
              <Show
                when={props.computers.length > 0}
                fallback={
                  <div class="side-empty">
                    <p class="side-empty__title">{t("settings.computers.empty.title")}</p>
                    <p class="side-empty__text">{t("settings.computers.empty.text")}</p>
                  </div>
                }
              >
                <div class="rows">
                  <For each={props.computers}>
                    {(computer) => (
                      <div class="row">
                        <span class="row__text">
                          <span class="row__name">{computer.name}</span>
                          <span class="row__meta">
                            {kindOf(computer)} · {sharingOf(computer)} ·{" "}
                            {computer.online ? t("chat.online") : t("chat.offline")}
                          </span>
                          <span class="row__meta mono">{computer.workspace}</span>
                          <span class="chips">
                            <Show when={computer.space}>
                              <span class="chip chip--default">{t("computer.space")}</span>
                            </Show>
                            <Show
                              when={botsOn(computer).length > 0}
                              fallback={
                                <span class="chip">{t("settings.computers.bots.none")}</span>
                              }
                            >
                              <For each={botsOn(computer)}>
                                {(bot) => (
                                  <button
                                    class="chip chip--button"
                                    type="button"
                                    onClick={() => props.onOpenBot(bot)}
                                  >
                                    {bot.name} · {accessOf(bot, computer)}
                                  </button>
                                )}
                              </For>
                            </Show>
                          </span>
                        </span>
                        {/* An exclusive Computer takes one Bot, so its owner
                            makes it shared before another may use it. */}
                        <Show
                          when={computer.kind === "hosted" && computer.sharing === "exclusive"}
                        >
                          <button
                            class="btn btn--quiet"
                            type="button"
                            disabled={props.busy}
                            onClick={() => void shareComputer(computer.id)}
                          >
                            {t("settings.computers.share")}
                          </button>
                        </Show>
                        {/* A reset clears a Hosted Workspace's files, and only a
                            Hosted Computer can be reset. */}
                        <Show when={computer.kind === "hosted"}>
                          <button
                            class={`btn ${
                              armedReset() === computer.id ? "btn--danger" : "btn--quiet"
                            }`}
                            type="button"
                            disabled={props.busy}
                            onClick={() => void reset(computer.id)}
                          >
                            <Icon name="repeat" size="sm" />
                            {armedReset() === computer.id
                              ? t("bot.computer.reset.armed")
                              : t("bot.computer.reset")}
                          </button>
                        </Show>
                      </div>
                    )}
                  </For>
                </div>
                <p class="panel__note">{t("bot.computer.reset.hint")}</p>
              </Show>

              <form
                class="panel__form"
                onSubmit={(event) => {
                  event.preventDefault();
                  void createComputer();
                }}
              >
                <div class="field-row">
                  <label class="field">
                    <span class="field__label">{t("settings.computers.create.name")}</span>
                    <input
                      class="input"
                      name="computer_name"
                      maxlength={64}
                      value={computerName()}
                      onInput={(event) => setComputerName(event.currentTarget.value)}
                    />
                  </label>
                  <label class="field">
                    <span class="field__label">{t("settings.computers.create.sharing")}</span>
                    <select
                      class="input"
                      name="computer_sharing"
                      value={computerSharing()}
                      onChange={(event) =>
                        setComputerSharing(event.currentTarget.value as Computer["sharing"])
                      }
                    >
                      <option value="shared">{t("computer.sharing.shared")}</option>
                      <option value="exclusive">{t("computer.sharing.exclusive")}</option>
                    </select>
                  </label>
                </div>
                <div class="panel__foot panel__foot--end">
                  <button
                    class="btn btn--primary"
                    type="submit"
                    disabled={props.busy || computerName().trim() === ""}
                  >
                    {props.busy
                      ? t("settings.computers.create.creating")
                      : t("settings.computers.create.action")}
                    <Icon name="arrow-right" size="sm" />
                  </button>
                </div>
                <span class="field__hint">{t("settings.computers.create.hint")}</span>
              </form>
            </div>
          </section>
        </Show>
      </div>
    </section>
  );
}
