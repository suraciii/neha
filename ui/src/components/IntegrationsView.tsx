import { createEffect, createSignal, For, Show } from "solid-js";

import { t } from "../i18n";
import type { Bot, InstalledPackage, McpServer } from "../types";
import Icon from "./Icon";

export interface IntegrationsViewProps {
  account: { id: string; displayName: string; isAdmin: boolean };
  bots: Bot[];
  servers: McpServer[];
  packages: InstalledPackage[];
  busy: boolean;
  error: string | null;
  // The URL of a sign-in the browser would not open. The view shows it as a
  // link while the page keeps polling the sign-in.
  signInUrl?: string | null;
  onAdd: (input: {
    name: string;
    url: string;
    scope: "space" | "user";
    token: string;
  }) => Promise<void>;
  onConnect: (server: string, token: string) => Promise<void>;
  onSignIn: (input: { name: string; url: string; scope: "space" | "user" }) => Promise<void>;
  // The form's scope changed, so anything it said about the old one is stale.
  onScopeChange?: () => void;
  onRemove: (server: string) => Promise<void>;
  onBot: (bot: string, server: string, enabled: boolean) => Promise<void>;
  onPackageEnabled: (plugin: string, enabled: boolean) => Promise<void>;
}

// One package's row: the metadata a caller could read, and the servers it
// brought. A member reads no package list, so the group keeps the servers and
// names the package only when the caller knows it.
interface PackageGroup {
  plugin: string;
  name: string | null;
  version: string | null;
  commit: string | null;
  enabled: boolean | null;
  servers: McpServer[];
}

// The two roots a stdio server's working directory hangs from, named as the
// record's placeholders: the record keeps the directory relative to its root.
const PLUGIN_ROOT = "${PLUGIN_ROOT}";
const PLUGIN_DATA = "${PLUGIN_DATA}";

// Runs one command the page owns. A refusal is reported through `error`, so the
// view does not repeat it.
function run(action: () => Promise<void>) {
  void action().catch(() => undefined);
}

// The Integrations view: the Space's and the User's MCP servers, the packages
// the Admin installed, and the Bots that use them. Every control it renders is
// one the Space would accept from this caller.
export default function IntegrationsView(props: IntegrationsViewProps) {
  const [name, setName] = createSignal("");
  const [url, setUrl] = createSignal("");
  const [scope, setScope] = createSignal<"space" | "user">("space");
  const [token, setToken] = createSignal("");
  // The server whose inline credential form is open, and the token typed into
  // it. One row opens at a time, and the token never outlives its submit.
  const [connecting, setConnecting] = createSignal<string | null>(null);
  const [connectToken, setConnectToken] = createSignal("");

  // Only the Admin names a Space server; every other account adds its own.
  const chosenScope = (): "space" | "user" => (props.account.isAdmin ? scope() : "user");

  // A Space server takes anyone's own credential; a User's server takes only
  // its owner's.
  function connectable(server: McpServer): boolean {
    return server.scope === "space" || server.by === props.account.id;
  }

  // The reader already holds a personal credential for this server.
  function personal(server: McpServer): boolean {
    return server.credential === "user" || server.credential === "space-and-user";
  }

  // Only the account that connected a server, or the Admin for the Space's,
  // removes it.
  function removable(server: McpServer): boolean {
    return server.by === props.account.id || (props.account.isAdmin && server.scope === "space");
  }

  // A stdio server is a command the Space launches on its own machine, so it
  // has no address: a browser sign-in has nowhere to go for one.
  function stdio(server: McpServer): boolean {
    return server.transport === "stdio" || server.url === null;
  }

  // The way a stdio server starts: the transport, the executable, and its
  // arguments, with the package's placeholders as the record keeps them.
  function commandLine(server: McpServer): string {
    const parts = ["stdio"];
    if (server.command) parts.push(server.command);
    parts.push(...(server.args ?? []));
    return parts.join(" ");
  }

  // A stdio server's working directory, named under the root it hangs from:
  // the record keeps only the relative path, so the same text means a
  // different directory under the package root and under its data directory.
  // An absent directory names the root itself, and the view shows none.
  function cwdLabel(server: McpServer): string | null {
    if (!server.cwd) return null;
    const root = server.data_cwd === true ? PLUGIN_DATA : PLUGIN_ROOT;
    return `${root}/${server.cwd}`;
  }

  // A browser sign-in is offered whenever the reader may use the server and a
  // working personal credential does not already replace it: none yet, only
  // the Space's (a personal one attaches over it), or one the Space refuses
  // (an expired credential to renew). Replacing a token is its own action. A
  // stdio server has no address to sign in to, so it is never offered.
  function signInOffered(server: McpServer): boolean {
    if (stdio(server)) return false;
    return connectable(server) && !(personal(server) && server.connected);
  }

  function ownerName(server: McpServer): string {
    return server.by === props.account.id ? t("integrations.you") : (server.by_name ?? server.by);
  }

  function scopeLabel(server: McpServer): string {
    return server.scope === "space" ? t("integrations.scope.space") : t("integrations.scope.user");
  }

  // The list is still on its way, so the view says it is loading instead of
  // saying it is empty.
  const loading = (): boolean => props.busy && props.servers.length === 0;

  const directServers = () => props.servers.filter((server) => server.plugin == null);

  // The package metadata is the Admin's to read. A member still sees the
  // servers a package brought: they group under the package's id, named by the
  // metadata when this caller may read it and neutrally otherwise.
  function packageGroups(): PackageGroup[] {
    const groups = new Map<string, PackageGroup>();
    if (props.account.isAdmin) {
      for (const pkg of props.packages) {
        groups.set(pkg.plugin, {
          plugin: pkg.plugin,
          name: pkg.name,
          version: pkg.version,
          commit: pkg.commit,
          enabled: pkg.enabled,
          servers: [],
        });
      }
    }
    for (const server of props.servers) {
      if (server.plugin == null) continue;
      let group = groups.get(server.plugin);
      if (!group) {
        group = {
          plugin: server.plugin,
          name: null,
          version: null,
          commit: null,
          enabled: null,
          servers: [],
        };
        groups.set(server.plugin, group);
      }
      group.servers.push(server);
    }
    return [...groups.values()];
  }

  // The token typed into the Add form belongs to the scope it was typed for.
  createEffect(
    () => scope(),
    () => {
      setToken("");
    },
  );

  // An inline credential form belongs to the list it was opened against: when
  // the Space's answer replaces that list, the form closes so its token cannot
  // edit whatever row holds that place now.
  createEffect(
    () => props.servers,
    () => {
      setConnecting(null);
      setConnectToken("");
    },
  );

  function addServer(event: SubmitEvent) {
    event.preventDefault();
    if (props.busy) return;
    const serverName = name().trim();
    const address = url().trim();
    if (serverName === "" || address === "") return;
    const credential = token();
    // Cleared before the command is sent, so a failed answer cannot leave the
    // token on the page.
    setToken("");
    run(() =>
      props.onAdd({ name: serverName, url: address, scope: chosenScope(), token: credential }),
    );
  }

  function signInNew() {
    if (props.busy) return;
    const serverName = name().trim();
    const address = url().trim();
    if (serverName === "" || address === "") return;
    run(() => props.onSignIn({ name: serverName, url: address, scope: chosenScope() }));
  }

  function signInExisting(server: McpServer) {
    if (props.busy) return;
    // The sign-in takes the scope the form selected: the Admin may attach the
    // Space's credential to a Space server, anyone else attaches their own. The
    // Space decides whether that scope is this caller's to name.
    const address = server.url;
    if (address === null) return;
    run(() => props.onSignIn({ name: server.name, url: address, scope: chosenScope() }));
  }

  function toggleConnect(server: McpServer) {
    if (props.busy) return;
    setConnectToken("");
    setConnecting((current) => (current === server.server ? null : server.server));
  }

  async function connect(event: SubmitEvent, server: McpServer) {
    event.preventDefault();
    if (props.busy) return;
    const credential = connectToken();
    if (credential === "") return;
    setConnectToken("");
    try {
      await props.onConnect(server.server, credential);
    } catch {
      // The page reports the refusal through `error`; the row keeps its word.
    } finally {
      setConnecting(null);
    }
  }

  function removeServer(server: McpServer) {
    if (props.busy) return;
    run(() => props.onRemove(server.server));
  }

  function togglePackage(group: PackageGroup) {
    if (props.busy || group.enabled === null) return;
    run(() => props.onPackageEnabled(group.plugin, !group.enabled));
  }

  function serverRow(server: McpServer) {
    return (
      <div class="library__row" data-server={server.server}>
        <div class="library__server">
          <div class="facts">
            <div class="fact">
              <span class="fact__label">{t("integrations.name")}</span>
              <span class="fact__value">
                {server.name}{" "}
                <Show
                  when={server.connected}
                  fallback={
                    <span class="state" title={t("integrations.notConnected.hint")}>
                      <span class="dot" />
                      {t("integrations.notConnected")}
                    </span>
                  }
                >
                  <span class="state state--done">
                    <span class="dot" />
                    {t("integrations.connected")}
                  </span>
                </Show>
              </span>
            </div>
            <Show
              when={stdio(server)}
              fallback={
                <div class="fact">
                  <span class="fact__label">{t("integrations.address")}</span>
                  <span class="fact__value mono">{server.url}</span>
                </div>
              }
            >
              <div class="fact">
                <span class="fact__label">{t("integrations.command")}</span>
                <span class="fact__value mono" data-command>
                  {commandLine(server)}
                </span>
              </div>
              <Show when={cwdLabel(server)}>
                {(cwd) => (
                  <div class="fact">
                    <span class="fact__label">{t("integrations.cwd")}</span>
                    <span class="fact__value mono">{cwd()}</span>
                  </div>
                )}
              </Show>
            </Show>
            <div class="fact">
              <span class="fact__label">{t("integrations.scope")}</span>
              <span class="fact__value">{scopeLabel(server)}</span>
            </div>
            <div class="fact">
              <span class="fact__label">{t("integrations.connectedBy")}</span>
              <span class="fact__value">{ownerName(server)}</span>
            </div>
          </div>
          <Show when={props.bots.length > 0}>
            {/* biome-ignore lint/a11y/useSemanticElements: the buttons are chips, not a form fieldset. */}
            <div class="chips" role="group" aria-label={t("integrations.bots")}>
              <For each={props.bots}>
                {(bot) => {
                  const uses = () => bot.mcps?.includes(server.server) === true;
                  return (
                    <label class="chip chip--button">
                      <input
                        type="checkbox"
                        checked={uses()}
                        disabled={props.busy}
                        aria-label={
                          uses()
                            ? t("integrations.takeAway")
                            : t("integrations.giveTo", { bot: bot.name })
                        }
                        onChange={(event) =>
                          run(() => props.onBot(bot.id, server.server, event.currentTarget.checked))
                        }
                      />
                      {bot.name}
                    </label>
                  );
                }}
              </For>
            </div>
          </Show>
          <Show when={connecting() === server.server}>
            <form class="field-row" onSubmit={(event) => void connect(event, server)}>
              <label class="field">
                <span class="field__label">{t("integrations.token")}</span>
                <input
                  class="input"
                  name="connect_token"
                  type="password"
                  autocomplete="off"
                  value={connectToken()}
                  onInput={(event) => setConnectToken(event.currentTarget.value)}
                />
              </label>
              <button class="btn btn--quiet" type="submit" disabled={props.busy}>
                {personal(server) ? t("integrations.replaceCredential") : t("integrations.connect")}
              </button>
            </form>
          </Show>
        </div>
        <div class="library__actions">
          <Show when={connectable(server)}>
            <button
              class="btn btn--quiet"
              type="button"
              disabled={props.busy}
              onClick={() => toggleConnect(server)}
            >
              {personal(server) ? t("integrations.replaceCredential") : t("integrations.connect")}
            </button>
          </Show>
          <Show when={signInOffered(server)}>
            <button
              class="btn btn--quiet"
              type="button"
              disabled={props.busy}
              onClick={() => signInExisting(server)}
            >
              {t("integrations.signIn")}
            </button>
          </Show>
          <Show when={removable(server)}>
            <button
              class="btn btn--danger"
              type="button"
              disabled={props.busy}
              onClick={() => removeServer(server)}
            >
              {t("integrations.remove")}
            </button>
          </Show>
        </div>
      </div>
    );
  }

  function packageGroup(group: PackageGroup) {
    return (
      <>
        <div class="library__row" data-package={group.plugin}>
          <div class="library__server">
            <div class="facts">
              <div class="fact">
                <span class="fact__label">{t("integrations.name")}</span>
                <span class="fact__value">{group.name ?? t("integrations.installedPackage")}</span>
              </div>
              <Show when={group.version}>
                {(version) => (
                  <div class="fact">
                    <span class="fact__label">{t("integrations.version")}</span>
                    <span class="fact__value mono">{version()}</span>
                  </div>
                )}
              </Show>
              <Show when={group.commit}>
                {(commit) => (
                  <div class="fact">
                    <span class="fact__label">{t("integrations.commit")}</span>
                    <span class="fact__value mono">{commit()}</span>
                  </div>
                )}
              </Show>
            </div>
          </div>
          <Show when={group.enabled !== null}>
            <div class="library__actions">
              <button
                class="btn btn--quiet"
                type="button"
                disabled={props.busy}
                onClick={() => togglePackage(group)}
              >
                {group.enabled ? t("integrations.disable") : t("integrations.enable")}
              </button>
            </div>
          </Show>
        </div>
        <For each={group.servers}>{(server) => serverRow(server)}</For>
      </>
    );
  }

  return (
    <div class="library">
      <h1>{t("integrations.title")}</h1>

      <Show when={props.error}>
        <p class="banner banner--error" role="alert">
          <Icon name="alert" size="sm" />
          {props.error}
        </p>
      </Show>

      <Show when={props.signInUrl}>
        {(address) => (
          <p class="banner" role="status" data-sign-in-fallback>
            <Icon name="link" size="sm" />
            <span>
              {t("integrations.signInBlocked")}{" "}
              <a href={address()} target="_blank" rel="noopener noreferrer">
                {address()}
              </a>
            </span>
          </p>
        )}
      </Show>

      <form class="panel" onSubmit={addServer}>
        <div class="panel__head">
          <div>
            <h2>{t("integrations.add.title")}</h2>
          </div>
        </div>
        <div class="panel__body">
          <div class="field-row">
            <label class="field">
              <span class="field__label">{t("integrations.name")}</span>
              <input
                class="input"
                name="server_name"
                maxlength={64}
                required
                value={name()}
                onInput={(event) => setName(event.currentTarget.value)}
              />
            </label>
            <label class="field">
              <span class="field__label">{t("integrations.address")}</span>
              <input
                class="input mono"
                name="server_url"
                inputmode="url"
                placeholder="https://mcp.example.com"
                required
                value={url()}
                onInput={(event) => setUrl(event.currentTarget.value)}
              />
            </label>
          </div>
          <Show when={props.account.isAdmin}>
            <label class="field">
              <span class="field__label">{t("integrations.scope")}</span>
              <select
                class="input"
                name="server_scope"
                value={scope()}
                onChange={(event) => {
                  // The form's scope decides what a credential is for: the
                  // token effect below clears the add form, and a row form
                  // open beside it is stale too.
                  setScope(event.currentTarget.value as "space" | "user");
                  setConnecting(null);
                  setConnectToken("");
                  props.onScopeChange?.();
                }}
              >
                <option value="space">{t("integrations.scope.space")}</option>
                <option value="user">{t("integrations.scope.user")}</option>
              </select>
            </label>
          </Show>
          <label class="field">
            <span class="field__label">{t("integrations.token")}</span>
            <input
              class="input"
              name="server_token"
              type="password"
              autocomplete="off"
              value={token()}
              onInput={(event) => setToken(event.currentTarget.value)}
            />
          </label>
        </div>
        <div class="panel__foot">
          <button class="btn btn--quiet" type="button" disabled={props.busy} onClick={signInNew}>
            {t("integrations.signInWithServer")}
          </button>
          <button class="btn btn--primary" type="submit" disabled={props.busy}>
            {t("integrations.add")}
            <Icon name="plus" size="sm" />
          </button>
        </div>
      </form>

      <Show
        when={!loading()}
        fallback={<p class="muted library__loading">{t("library.loading")}</p>}
      >
        <Show when={packageGroups().length > 0 || props.account.isAdmin}>
          <section
            class="library__group"
            data-group="packages"
            aria-label={t("integrations.packages")}
          >
            <div class="library__head">
              <h2>{t("integrations.packages")}</h2>
            </div>
            <Show
              when={packageGroups().length > 0}
              fallback={<p class="muted">{t("integrations.noPackages")}</p>}
            >
              <For each={packageGroups()}>{(group) => packageGroup(group)}</For>
            </Show>
          </section>
        </Show>

        <section class="library__group" data-group="direct" aria-label={t("integrations.direct")}>
          <div class="library__head">
            <h2>{t("integrations.direct")}</h2>
          </div>
          <Show
            when={directServers().length > 0}
            fallback={<p class="muted">{t("integrations.empty")}</p>}
          >
            <For each={directServers()}>{(server) => serverRow(server)}</For>
          </Show>
        </section>
      </Show>
    </div>
  );
}
