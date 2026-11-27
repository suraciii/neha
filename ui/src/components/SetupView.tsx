import { createSignal, Show } from "solid-js";

import { t } from "../i18n";
import type { Account, Bot, Model, Provider } from "../types";
import Icon from "./Icon";
import LocaleSwitch from "./LocaleSwitch";

interface ProviderInput {
  baseUrl: string;
  apiKey: string;
  model: string;
  contextLimit: number;
  maxOutput: number;
}

interface SetupViewProps {
  account: Account;
  providers: Provider[];
  models: Model[];
  bots: Bot[];
  busy: boolean;
  error: string | null;
  onProvider: (input: ProviderInput) => void;
  onBot: (name: string) => void;
}

export default function SetupView(props: SetupViewProps) {
  const [baseUrl, setBaseUrl] = createSignal("");
  const [apiKey, setApiKey] = createSignal("");
  const [model, setModel] = createSignal("");
  const [contextLimit, setContextLimit] = createSignal(128_000);
  const [maxOutput, setMaxOutput] = createSignal(8_192);
  const [botName, setBotName] = createSignal("");
  const [showAdvanced, setShowAdvanced] = createSignal(false);

  const providerReady = () => props.providers.length > 0;

  function connectProvider(event: SubmitEvent) {
    event.preventDefault();
    if (props.busy) return;
    props.onProvider({
      baseUrl: baseUrl().trim(),
      apiKey: apiKey(),
      model: model().trim(),
      contextLimit: contextLimit(),
      maxOutput: maxOutput(),
    });
  }

  function createBot(event: SubmitEvent) {
    event.preventDefault();
    if (props.busy || botName().trim() === "") return;
    props.onBot(botName().trim());
  }

  return (
    <div class="setup">
      <header class="setup__head">
        <div class="setup__top">
          <LocaleSwitch class="setup__locale" />
        </div>
        <h1>{providerReady() ? t("setup.bot.title") : t("setup.provider.title")}</h1>
        <p class="setup__lede">
          {providerReady() ? t("setup.bot.lede") : t("setup.provider.lede")}
        </p>
        <div class="setup__progress-row">
          <div
            class="setup__progress"
            role="progressbar"
            aria-valuemin={1}
            aria-valuemax={2}
            aria-valuenow={providerReady() ? 2 : 1}
            aria-valuetext={providerReady() ? t("setup.step.two") : t("setup.step.one")}
          >
            <span class="setup__progress-seg is-on" />
            <span class={`setup__progress-seg ${providerReady() ? "is-on" : ""}`} />
          </div>
          <span class="setup__progress-text">
            {providerReady() ? t("setup.step.two") : t("setup.step.one")}
          </span>
        </div>
      </header>

      <Show when={props.error}>
        <p class="banner banner--error" role="alert">
          <Icon name="alert" size="sm" />
          {props.error}
        </p>
      </Show>

      <Show
        when={!providerReady()}
        fallback={
          <>
            <section class="panel">
              <div class="panel__head">
                <div>
                  <h2>
                    {props.providers.length === 1
                      ? t("setup.provider.connected.one")
                      : t("setup.provider.connected.many", { count: props.providers.length })}
                  </h2>
                </div>
                <span class="state state--done">
                  <span class="dot" />
                  {t("setup.ready")}
                </span>
              </div>
              <div class="panel__body">
                <div class="facts">
                  <div class="fact">
                    <span class="fact__label">{t("setup.baseUrl")}</span>
                    <span class="fact__value mono">{props.providers[0]?.base_url}</span>
                  </div>
                  <div class="fact">
                    <span class="fact__label">{t("setup.models")}</span>
                    <span class="fact__value">
                      {props.models.map((item) => item.name).join(", ") || t("setup.none")}
                    </span>
                  </div>
                </div>
              </div>
            </section>

            <form class="panel" onSubmit={createBot}>
              <div class="panel__head">
                <div>
                  <h2>{t("setup.bot.panelTitle")}</h2>
                </div>
              </div>
              <div class="panel__body">
                <label class="field">
                  <span class="field__label">{t("setup.bot.name")}</span>
                  <input
                    class="input"
                    name="bot_name"
                    maxlength={64}
                    placeholder="Scout"
                    required
                    value={botName()}
                    onInput={(event) => setBotName(event.currentTarget.value)}
                  />
                  <span class="field__hint">{t("setup.bot.nameHint")}</span>
                </label>
                <div class="access-row">
                  <span class="access-row__icon">
                    <Icon name="lock" size="sm" />
                  </span>
                  <span>
                    <b>{t("setup.bot.accessTitle")}</b>
                    <span>{t("setup.bot.accessHint")}</span>
                  </span>
                </div>
              </div>
              <div class="panel__foot">
                <button
                  class="btn btn--primary"
                  type="submit"
                  disabled={props.busy || botName().trim() === ""}
                >
                  {props.busy ? t("setup.bot.creating") : t("setup.bot.create")}
                  <Icon name="arrow-right" size="sm" />
                </button>
              </div>
            </form>
          </>
        }
      >
        <Show when={props.account.is_admin} fallback={<p class="banner">{t("setup.noAdmin")}</p>}>
          <form class="panel" onSubmit={connectProvider}>
            <div class="panel__head">
              <div>
                <h2>{t("setup.provider.panelTitle")}</h2>
              </div>
              <span class="badge">{t("setup.adminOnly")}</span>
            </div>
            <div class="panel__body">
              <label class="field">
                <span class="field__label">{t("setup.baseUrl")}</span>
                <input
                  class="input mono"
                  name="base_url"
                  inputmode="url"
                  placeholder="http://127.0.0.1:8000/v1"
                  required
                  value={baseUrl()}
                  onInput={(event) => setBaseUrl(event.currentTarget.value)}
                />
                <span class="field__hint">{t("setup.baseUrl.hint")}</span>
              </label>
              <label class="field">
                <span class="field__label">{t("setup.apiKey")}</span>
                <input
                  class="input"
                  name="api_key"
                  type="password"
                  autocomplete="off"
                  required
                  value={apiKey()}
                  onInput={(event) => setApiKey(event.currentTarget.value)}
                />
                <span class="field__hint">{t("setup.apiKey.hint")}</span>
              </label>
              <label class="field">
                <span class="field__label">{t("setup.model")}</span>
                <input
                  class="input mono"
                  name="model"
                  placeholder="example-chat"
                  required
                  value={model()}
                  onInput={(event) => setModel(event.currentTarget.value)}
                />
                <span class="field__hint">{t("setup.model.hint")}</span>
              </label>

              <button
                class="link-btn"
                type="button"
                aria-expanded={showAdvanced() ? "true" : "false"}
                onClick={() => setShowAdvanced((open) => !open)}
              >
                <Icon name={showAdvanced() ? "chevron-up" : "chevron-down"} size="sm" />
                {t("setup.advanced")}
              </button>
              <Show when={showAdvanced()}>
                <div class="field-row">
                  <label class="field">
                    <span class="field__label">{t("setup.contextLimit")}</span>
                    <input
                      class="input mono"
                      type="number"
                      min="1"
                      value={contextLimit()}
                      onInput={(event) => setContextLimit(Number(event.currentTarget.value))}
                    />
                  </label>
                  <label class="field">
                    <span class="field__label">{t("setup.maxOutput")}</span>
                    <input
                      class="input mono"
                      type="number"
                      min="1"
                      value={maxOutput()}
                      onInput={(event) => setMaxOutput(Number(event.currentTarget.value))}
                    />
                  </label>
                </div>
              </Show>
            </div>
            <div class="panel__foot">
              <span class="panel__note">{t("setup.provider.note")}</span>
              <button class="btn btn--primary" type="submit" disabled={props.busy}>
                {props.busy ? t("setup.connecting") : t("setup.connect")}
                <Icon name="arrow-right" size="sm" />
              </button>
            </div>
          </form>
        </Show>
      </Show>
    </div>
  );
}
