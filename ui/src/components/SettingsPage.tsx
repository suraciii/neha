import { For, Show, createSignal } from "solid-js";

import { locale, t, type MessageKey } from "../i18n";
import { dayLabel } from "../lib/format";
import type {
  Account,
  CatalogStatus,
  CatalogVendor,
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
  // Every vendor the catalog knows that this Space can connect.
  vendors: CatalogVendor[];
  catalog: CatalogStatus | null;
  section: SettingsSection;
  // A write is on its way to the Space.
  busy: boolean;
  error: string | null;
  onBack: () => void;
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
  const [protocol, setProtocol] = createSignal<string>(PROTOCOLS[0]);
  const [thinking, setThinking] = createSignal<string>("");
  const [advanced, setAdvanced] = createSignal(false);
  // What the last accepted change did, so a quiet write still answers.
  const [note, setNote] = createSignal<string | null>(null);

  const canConnect = () =>
    props.account.is_admin && !props.busy && vendor() !== "" && apiKey() !== "";
  const connectable = () => props.vendors.filter((known) => known.selectable);

  async function connect() {
    if (!canConnect()) return;
    setNote(null);
    const done = await props.onConnect({
      vendor: vendor(),
      apiKey: apiKey(),
      baseUrl: baseUrl().trim() === "" ? null : baseUrl().trim(),
      protocol: advanced() ? protocol() : null,
      thinking: advanced() && thinking() !== "" ? (thinking() as ThinkingLevel) : null,
    });
    // The key is never shown again, so the form starts over either way.
    setApiKey("");
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
        : t("sidebar.settings.catalog");

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
                      onChange={(event) => setVendor(event.currentTarget.value)}
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
                when={props.models.length > 0}
                fallback={
                  <div class="side-empty">
                    <p class="side-empty__title">{t("settings.models.empty.title")}</p>
                    <p class="side-empty__text">{t("settings.models.empty.text")}</p>
                  </div>
                }
              >
                <div class="rows">
                  <For each={props.models}>
                    {(model) => (
                      <div class="row">
                        <span class="row__text">
                          <span class="row__name mono">{model.name}</span>
                          <span class="row__meta">
                            {props.providers.find((known) => known.id === model.provider)?.base_url ?? ""}
                          </span>
                          <span class="chips">
                            <span class="chip">{t("bot.model.context", { limit: model.context_limit })}</span>
                            <span class="chip">{t("bot.model.output", { limit: model.max_output })}</span>
                            <span class="chip">
                              {model.reads_images ? t("bot.model.readsImages") : t("bot.model.noImages")}
                            </span>
                            <span class="chip">
                              {model.tools ? t("bot.model.callsTools") : t("bot.model.noTools")}
                            </span>
                            <Show when={model.is_default}>
                              <span class="chip chip--default">{t("settings.models.default")}</span>
                            </Show>
                          </span>
                        </span>
                        <Show when={props.account.is_admin && !model.is_default}>
                          <button
                            class="btn btn--quiet"
                            type="button"
                            disabled={props.busy}
                            onClick={() => void makeDefault(model.id)}
                          >
                            {t("settings.models.setDefault")}
                          </button>
                        </Show>
                      </div>
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
      </div>
    </section>
  );
}
