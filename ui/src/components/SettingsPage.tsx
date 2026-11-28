import qrcode from "qrcode-generator";
import { createMemo, createSignal, For, Show } from "solid-js";

import { locale, type MessageKey, t } from "../i18n";
import { dayLabel, timeOfDay } from "../lib/format";
import { reachesOnlyLocally } from "../lib/reach";
import type {
  Account,
  Bot,
  CatalogStatus,
  CatalogVendor,
  Computer,
  CreatedInvitation,
  Invitation,
  Model,
  Provider,
  Role,
  ThinkingLevel,
  User,
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

// What each role is called in the User's words.
const ROLE_LABEL: Record<Role, MessageKey> = {
  owner: "settings.people.role.owner",
  admin: "settings.people.role.admin",
  member: "settings.people.role.member",
};

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
  // The Space's Users, in the order they joined, and the one invitation that
  // still admits a User. A Member reads the Users; only the Space Owner and the
  // Admins read the invitation.
  users: User[];
  invitation: Invitation | null;
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
  // Adds a Local Computer whose machine printed a pairing code.
  onAdd: (code: string) => Promise<boolean>;
  // Revokes a Local Computer's credential and disconnects its daemon.
  onRevoke: (computer: string) => Promise<boolean>;
  // Approves a CLI's device code, signing that terminal in as this account.
  onApproveDevice: (code: string) => Promise<boolean>;
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
  // Makes an invitation and answers it once, so its link and QR code show in
  // the panel that creates them. The Space never reads the link back.
  onInvite: () => Promise<CreatedInvitation | null>;
  // Revokes the pending invitation.
  onRevokeInvitation: (invitation: string) => Promise<boolean>;
  // Changes another User's role; only the Space Owner may.
  onSetRole: (user: string, role: "admin" | "member") => Promise<boolean>;
  // Gives the Space to another User; only the Space Owner may.
  onTransfer: (user: string) => Promise<boolean>;
}

// What each role is called in the User's words.
function roleLabel(role: Role): string {
  return t(ROLE_LABEL[role]);
}

// The invitation link as a QR code, drawn from the code's own module grid. The
// SVG keeps the light ground and the quiet zone a scanner expects, and the
// modules stay crisp at any size.
function InvitationQr(props: { link: string }) {
  const code = createMemo(() => {
    const qr = qrcode(0, "M");
    qr.addData(props.link);
    qr.make();
    const count = qr.getModuleCount();
    // The quiet zone around the grid, in modules, keeps a camera's finder
    // pattern clear of the page behind the code.
    const quiet = 4;
    const modules: string[] = [];
    for (let row = 0; row < count; row += 1) {
      for (let col = 0; col < count; col += 1) {
        if (qr.isDark(row, col)) modules.push(`M${col + quiet} ${row + quiet}h1v1h-1z`);
      }
    }
    return { size: count + quiet * 2, path: modules.join("") };
  });
  return (
    <svg
      class="invite__qr"
      role="img"
      aria-label={t("settings.people.qr")}
      viewBox={`0 0 ${code().size} ${code().size}`}
      width="200"
      height="200"
      shape-rendering="crispEdges"
    >
      <rect class="invite__qr-ground" width={code().size} height={code().size} />
      <path class="invite__qr-modules" d={code().path} />
    </svg>
  );
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
  // The invitation this page last made, with the secret it showed. The Space
  // answers a create once, so the page keeps the link only until a reload or
  // the next create, which replaces it.
  const [created, setCreated] = createSignal<CreatedInvitation | null>(null);
  const [copied, setCopied] = createSignal(false);
  // The invitation whose revoke the Owner or an Admin armed with a first press,
  // and the User a transfer was armed for. Deleting and transferring change who
  // the Space is, so both ask twice.
  const [armedRevoke, setArmedRevoke] = createSignal<string | null>(null);
  const [armedTransfer, setArmedTransfer] = createSignal<string | null>(null);

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

  async function approveDevice() {
    if (props.busy || deviceCode().trim() === "") return;
    setNote(null);
    if (await props.onApproveDevice(deviceCode().trim())) {
      setDeviceCode("");
      setNote(t("settings.devices.approved"));
    }
  }

  // What the signed-in account may do here: the Space Owner and the Admins make
  // and revoke invitations, and only the Space Owner changes a role or
  // transfers the Space.
  const administers = () => props.account.is_admin;
  const owns = () => props.account.role === "owner";
  // The one-time link the page last created. A second create replaces it, a
  // reload forgets it, and a revoke or a used invitation takes it away: the
  // link only lives while it is the pending invitation.
  const createdLink = () => {
    const answer = created();
    if (answer === null || typeof answer.secret !== "string") return null;
    if (props.invitation?.id !== answer.invitation.id) return null;
    return `${location.origin}/invite#${answer.secret}`;
  };
  // A Local Space's link reaches only the devices that can reach this machine.
  const localReach = () => reachesOnlyLocally(location.hostname);
  // When the pending invitation stops working, in the reader's words. An
  // invitation lasts a week, so the day and the time are enough.
  const expiry = (invitation: Invitation) =>
    `${dayLabel(invitation.expires_at, locale())} · ${timeOfDay(invitation.expires_at)}`;

  async function invite() {
    if (props.busy) return;
    // A second create replaces the first answer, so the link it showed leaves
    // with it.
    setCreated(null);
    setCopied(false);
    setNote(null);
    const answer = await props.onInvite();
    if (answer === null) return;
    if (typeof answer.secret !== "string") {
      // The Space answered from a recorded command, which keeps no secret; a
      // new link is the only way to show one.
      setNote(t("settings.people.invite.again"));
      return;
    }
    setCreated(answer);
  }

  async function copy() {
    const link = createdLink();
    if (link === null) return;
    try {
      await navigator.clipboard.writeText(link);
      setCopied(true);
    } catch {
      // A browser that refuses the clipboard still shows the link; the reader
      // selects it from the field.
      setNote(t("settings.people.copy.failed"));
    }
  }

  async function revoke(invitation: string) {
    if (props.busy) return;
    if (armedRevoke() !== invitation) {
      setArmedRevoke(invitation);
      return;
    }
    setArmedRevoke(null);
    setNote(null);
    if (await props.onRevokeInvitation(invitation)) setNote(t("settings.people.revoke.done"));
  }

  async function changeRole(user: User, role: "admin" | "member") {
    if (props.busy) return;
    setNote(null);
    if (await props.onSetRole(user.id, role)) setNote(t("settings.people.role.done"));
  }

  async function transfer(user: User) {
    if (props.busy) return;
    if (armedTransfer() !== user.id) {
      setArmedTransfer(user.id);
      return;
    }
    setArmedTransfer(null);
    setNote(null);
    if (await props.onTransfer(user.id)) setNote(t("settings.people.transfer.done"));
  }

  const title = () =>
    props.section === "providers"
      ? t("sidebar.settings.providers")
      : props.section === "models"
        ? t("sidebar.settings.models")
        : props.section === "computers"
          ? t("sidebar.settings.computers")
          : props.section === "devices"
            ? t("sidebar.settings.devices")
            : props.section === "people"
              ? t("sidebar.settings.people")
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
  const [computerSharing, setComputerSharing] = createSignal<Computer["sharing"]>("exclusive");
  // The code a machine on another machine showed. It is entered on any
  // signed-in client, which approves and records that machine.
  const [pairingCode, setPairingCode] = createSignal("");
  // Revoking access disconnects a machine, so it asks twice.
  const [armedComputerRevoke, setArmedComputerRevoke] = createSignal<string | null>(null);
  // The code a terminal without a session shows.
  const [deviceCode, setDeviceCode] = createSignal("");

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

  async function addComputer() {
    const code = pairingCode().trim();
    if (code === "" || props.busy) return;
    setNote(null);
    if (await props.onAdd(code)) {
      setPairingCode("");
      setNote(t("settings.computers.add.done"));
    }
  }

  async function revokeComputer(computer: string) {
    if (props.busy) return;
    if (armedComputerRevoke() !== computer) {
      setArmedComputerRevoke(computer);
      return;
    }
    setArmedComputerRevoke(null);
    setNote(null);
    if (await props.onRevoke(computer)) setNote(t("settings.computers.revoke.done"));
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
        <button
          class="page__back"
          type="button"
          onClick={props.onBack}
          aria-label={t("sidebar.settings")}
        >
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
                            {props.busy
                              ? t("settings.providers.refreshing")
                              : t("settings.providers.refresh")}
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
                      {connectable().length === 0
                        ? t("settings.providers.vendor.empty")
                        : t("setup.provider.lede")}
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
                          placeholder={
                            props.vendors.find((known) => known.id === vendor())?.api ?? ""
                          }
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
                        {catalog().stale
                          ? t("settings.catalog.stale")
                          : t("settings.catalog.fresh")}
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
                        <Show when={computer.kind === "hosted" && computer.sharing === "exclusive"}>
                          <button
                            class="btn btn--quiet"
                            type="button"
                            disabled={props.busy}
                            onClick={() => void shareComputer(computer.id)}
                          >
                            {t("settings.computers.share")}
                          </button>
                        </Show>
                        {/* Revoking access disconnects a machine; only a
                            Local Computer holds a credential to revoke. */}
                        <Show when={computer.kind === "local"}>
                          <button
                            class={`btn ${
                              armedComputerRevoke() === computer.id ? "btn--danger" : "btn--quiet"
                            }`}
                            type="button"
                            disabled={props.busy}
                            onClick={() => void revokeComputer(computer.id)}
                          >
                            <Icon name="x" size="sm" />
                            {armedComputerRevoke() === computer.id
                              ? t("settings.computers.revoke.armed", { name: computer.name })
                              : t("settings.computers.revoke")}
                          </button>
                          <Show when={armedComputerRevoke() === computer.id}>
                            <span class="field__hint">{t("settings.computers.revoke.hint")}</span>
                          </Show>
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

              <form
                class="panel__form"
                onSubmit={(event) => {
                  event.preventDefault();
                  void addComputer();
                }}
              >
                <h3 class="panel__subtitle">{t("settings.computers.add.title")}</h3>
                <p class="panel__note">{t("settings.computers.add.hint")}</p>
                <div class="field-row">
                  <label class="field">
                    <span class="field__label">{t("settings.computers.add.code")}</span>
                    <input
                      class="input mono"
                      name="pairing_code"
                      autocomplete="off"
                      spellcheck={false}
                      maxlength={16}
                      placeholder={t("settings.computers.add.placeholder")}
                      value={pairingCode()}
                      onInput={(event) => setPairingCode(event.currentTarget.value)}
                    />
                  </label>
                </div>
                <div class="panel__foot panel__foot--end">
                  <button
                    class="btn btn--primary"
                    type="submit"
                    disabled={props.busy || pairingCode().trim() === ""}
                  >
                    {props.busy
                      ? t("settings.computers.add.adding")
                      : t("settings.computers.add.action")}
                    <Icon name="arrow-right" size="sm" />
                  </button>
                </div>
              </form>
            </div>
          </section>
        </Show>

        <Show when={props.section === "devices"}>
          <section class="panel">
            <div class="panel__head">
              <div>
                <h2>{t("sidebar.settings.devices")}</h2>
              </div>
            </div>
            <div class="panel__body">
              <p class="panel__note">{t("settings.devices.hint")}</p>
              <form
                class="panel__form"
                onSubmit={(event) => {
                  event.preventDefault();
                  void approveDevice();
                }}
              >
                <div class="field-row">
                  <label class="field">
                    <span class="field__label">{t("settings.devices.code")}</span>
                    <input
                      class="input mono"
                      name="device_code"
                      autocomplete="off"
                      spellcheck={false}
                      maxlength={16}
                      placeholder={t("settings.devices.placeholder")}
                      value={deviceCode()}
                      onInput={(event) => setDeviceCode(event.currentTarget.value)}
                    />
                  </label>
                </div>
                <div class="panel__foot panel__foot--end">
                  <button
                    class="btn btn--primary"
                    type="submit"
                    disabled={props.busy || deviceCode().trim() === ""}
                  >
                    {props.busy ? t("settings.devices.approving") : t("settings.devices.action")}
                    <Icon name="arrow-right" size="sm" />
                  </button>
                </div>
                <span class="field__hint">{t("settings.devices.warning")}</span>
              </form>
            </div>
          </section>
        </Show>

        <Show when={props.section === "people"}>
          <section class="panel">
            <div class="panel__head">
              <div>
                <h2>{t("sidebar.settings.people")}</h2>
              </div>
              <span class="badge">{props.users.length}</span>
            </div>
            <div class="panel__body">
              <p class="panel__note">{t("settings.people.hint")}</p>
              <Show
                when={props.users.length > 0}
                fallback={<p class="rows__empty">{t("common.loading")}</p>}
              >
                <div class="rows">
                  <For each={props.users}>
                    {(user) => (
                      <div class="row">
                        <span class="row__text">
                          <span class="row__name">
                            {user.display_name}
                            <Show when={user.id === props.account.id}>
                              <span class="row__meta"> {t("settings.people.you")}</span>
                            </Show>
                          </span>
                          <span class="row__meta">{roleLabel(user.role)}</span>
                        </span>
                        {/* The Space Owner's role changes only by transfer, and
                            a User never changes its own: both controls stay off
                            the Owner's row and the reader's own. */}
                        <Show
                          when={owns() && user.role !== "owner" && user.id !== props.account.id}
                        >
                          <button
                            class="btn btn--quiet"
                            type="button"
                            disabled={props.busy}
                            onClick={() =>
                              void changeRole(user, user.role === "admin" ? "member" : "admin")
                            }
                          >
                            {user.role === "admin"
                              ? t("settings.people.role.makeMember")
                              : t("settings.people.role.makeAdmin")}
                          </button>
                          <button
                            class={`btn ${
                              armedTransfer() === user.id ? "btn--danger" : "btn--quiet"
                            }`}
                            type="button"
                            disabled={props.busy}
                            onClick={() => void transfer(user)}
                          >
                            <Icon name="arrow-right" size="sm" />
                            {armedTransfer() === user.id
                              ? t("settings.people.transfer.armed", { name: user.display_name })
                              : t("settings.people.transfer")}
                          </button>
                        </Show>
                      </div>
                    )}
                  </For>
                </div>
              </Show>

              <Show when={administers()}>
                {/* The invitation that still admits a User, and the way to take
                    it back. A Member reads the Users list alone. */}
                <Show when={props.invitation}>
                  {(invitation) => (
                    <div class="row">
                      <span class="row__text">
                        <span class="row__name">{t("settings.people.pending")}</span>
                        <span class="row__meta">
                          {t("settings.people.pending.expires", {
                            time: expiry(invitation()),
                          })}
                        </span>
                      </span>
                      <button
                        class={`btn ${
                          armedRevoke() === invitation().id ? "btn--danger" : "btn--quiet"
                        }`}
                        type="button"
                        disabled={props.busy}
                        onClick={() => void revoke(invitation().id)}
                      >
                        <Icon name="trash" size="sm" />
                        {armedRevoke() === invitation().id
                          ? t("settings.people.revoke.armed")
                          : t("settings.people.revoke")}
                      </button>
                    </div>
                  )}
                </Show>

                {/* The one answer a create gives: the link, the way to copy it,
                    and the code a phone scans. It leaves on the next create or
                    on a reload. */}
                <Show when={createdLink()}>
                  {(link) => (
                    <section class="invite" aria-label={t("settings.people.invite.link")}>
                      <div class="invite__details">
                        <div class="field">
                          <span class="field__label">{t("settings.people.invite.link")}</span>
                          {/* The whole link stays readable: it wraps rather
                              than scrolling out of view, and one press selects
                              it for a browser that refuses the clipboard. */}
                          <code class="invite__link mono">{link()}</code>
                        </div>
                        <p class="panel__note">{t("settings.people.invite.once")}</p>
                        <p class="panel__note">{t("settings.people.invite.member")}</p>
                        <Show when={localReach()}>
                          <p class="panel__note">{t("settings.people.invite.local")}</p>
                        </Show>
                        <div class="invite__actions">
                          <button
                            class="btn btn--primary"
                            type="button"
                            onClick={() => void copy()}
                          >
                            <Icon name="copy" size="sm" />
                            {copied() ? t("settings.people.copied") : t("settings.people.copy")}
                          </button>
                        </div>
                      </div>
                      <InvitationQr link={link()} />
                    </section>
                  )}
                </Show>

                <div class="panel__foot panel__foot--end">
                  <button
                    class="btn btn--primary"
                    type="button"
                    disabled={props.busy}
                    onClick={() => void invite()}
                  >
                    <Icon name="plus" size="sm" />
                    {props.busy
                      ? t("settings.people.invite.creating")
                      : t("settings.people.invite")}
                  </button>
                </div>
                <p class="panel__note">{t("settings.people.invite.hint")}</p>
                <Show when={localReach()}>
                  <p class="panel__note">{t("settings.people.invite.local")}</p>
                </Show>
              </Show>
            </div>
          </section>
        </Show>
      </div>
    </section>
  );
}
