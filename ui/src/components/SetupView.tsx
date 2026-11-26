import { Show, createSignal } from "solid-js";

import type { Account, Bot, Model, Provider } from "../types";
import Icon from "./Icon";

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
        <p class="eyebrow">{providerReady() ? "Step 2 of 2" : "Step 1 of 2"}</p>
        <h1>{providerReady() ? "Create your first Bot" : "Connect a model Provider first"}</h1>
        <p class="setup__lede">
          {providerReady()
            ? "A Bot has its own identity, memory, and Computer. Give it a name and hand it work."
            : "Every Bot in this Space thinks through a Provider. The key stays inside the Space."}
        </p>
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
                  <p class="panel__kicker">Provider</p>
                  <h2>{props.providers.length === 1 ? "Provider connected" : `${props.providers.length} Providers connected`}</h2>
                </div>
                <span class="state state--done">
                  <span class="dot" />
                  Ready
                </span>
              </div>
              <div class="panel__body">
                <div class="facts">
                  <div class="fact">
                    <span class="fact__label">Base URL</span>
                    <span class="fact__value mono">{props.providers[0]?.base_url}</span>
                  </div>
                  <div class="fact">
                    <span class="fact__label">Models</span>
                    <span class="fact__value">{props.models.map((item) => item.name).join(", ") || "None"}</span>
                  </div>
                </div>
              </div>
            </section>

            <form class="panel" onSubmit={createBot}>
              <div class="panel__head">
                <div>
                  <p class="panel__kicker">Bot</p>
                  <h2>Name the teammate</h2>
                </div>
                <span class="badge">Owner only</span>
              </div>
              <div class="panel__body">
                <label class="field">
                  <span class="field__label">Name</span>
                  <input
                    class="input"
                    name="bot_name"
                    maxlength={64}
                    placeholder="Scout"
                    required
                    value={botName()}
                    onInput={(event) => setBotName(event.currentTarget.value)}
                  />
                  <span class="field__hint">The name appears in Chats, Groups, and the task board.</span>
                </label>
                <div class="access-row">
                  <span class="access-row__icon">
                    <Icon name="lock" size="sm" />
                  </span>
                  <span>
                    <b>Owner only</b>
                    <span>Only you can use this Bot. You can invite members later.</span>
                  </span>
                </div>
              </div>
              <div class="panel__foot">
                <button class="btn btn--primary" type="submit" disabled={props.busy || botName().trim() === ""}>
                  {props.busy ? "Creating…" : "Create Bot"}
                  <Icon name="arrow-right" size="sm" />
                </button>
              </div>
            </form>
          </>
        }
      >
        <Show
          when={props.account.is_admin}
          fallback={
            <p class="banner">Ask the Space Admin to connect a Provider before a Bot can work.</p>
          }
        >
          <form class="panel" onSubmit={connectProvider}>
            <div class="panel__head">
              <div>
                <p class="panel__kicker">Provider</p>
                <h2>Connect an OpenAI-compatible Provider</h2>
              </div>
              <span class="badge">Admin only</span>
            </div>
            <div class="panel__body">
              <label class="field">
                <span class="field__label">Base URL</span>
                <input
                  class="input mono"
                  name="base_url"
                  inputmode="url"
                  placeholder="http://127.0.0.1:8000/v1"
                  required
                  value={baseUrl()}
                  onInput={(event) => setBaseUrl(event.currentTarget.value)}
                />
                <span class="field__hint">The address that serves OpenAI Chat Completions.</span>
              </label>
              <label class="field">
                <span class="field__label">API key</span>
                <input
                  class="input"
                  name="api_key"
                  type="password"
                  autocomplete="off"
                  required
                  value={apiKey()}
                  onInput={(event) => setApiKey(event.currentTarget.value)}
                />
                <span class="field__hint">Stored sealed in this Space. The browser never reads it back.</span>
              </label>
              <label class="field">
                <span class="field__label">Model name</span>
                <input
                  class="input mono"
                  name="model"
                  placeholder="example-chat"
                  required
                  value={model()}
                  onInput={(event) => setModel(event.currentTarget.value)}
                />
                <span class="field__hint">The exact name the Provider expects.</span>
              </label>

              <button
                class="link-btn"
                type="button"
                aria-expanded={showAdvanced()}
                onClick={() => setShowAdvanced((open) => !open)}
              >
                <Icon name={showAdvanced() ? "chevron-up" : "chevron-down"} size="sm" />
                Advanced
              </button>
              <Show when={showAdvanced()}>
                <div class="field-row">
                  <label class="field">
                    <span class="field__label">Context limit</span>
                    <input
                      class="input mono"
                      type="number"
                      min="1"
                      value={contextLimit()}
                      onInput={(event) => setContextLimit(Number(event.currentTarget.value))}
                    />
                  </label>
                  <label class="field">
                    <span class="field__label">Max output</span>
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
              <span class="panel__note">A Provider belongs to the whole Space, not to one Browser.</span>
              <button class="btn btn--primary" type="submit" disabled={props.busy}>
                {props.busy ? "Connecting…" : "Connect Provider"}
                <Icon name="arrow-right" size="sm" />
              </button>
            </div>
          </form>
        </Show>
      </Show>
    </div>
  );
}
