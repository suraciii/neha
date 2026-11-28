import { createSignal, createTrackedEffect, For, Show, untrack } from "solid-js";

import { t } from "../i18n";
import { formatBytes } from "../lib/files";
import type {
  Bot,
  Computer,
  MemoryEntry,
  MemoryKind,
  MemoryList,
  MemoryScope,
  Model,
  Rule,
  ThinkingLevel,
} from "../types";
import Avatar from "./Avatar";
import Icon from "./Icon";

// How much thought a Model can give, cheapest first.
const LEVELS: ThinkingLevel[] = ["minimal", "low", "medium", "high", "max"];

// The tools every Bot has. A rule names one of these, so the page offers the
// set the execution side runs rather than a name it would refuse.
const TOOLS = ["shell", "read_file", "write_file", "list_files"];

// The kinds of memory entry, in the order the page groups them: the lasting
// rules first, then the notes the Bot keeps for itself.
const KINDS: MemoryKind[] = ["rule", "note"];

// The scopes a memory entry applies in, in the order the page groups them: the
// owner's Chats, one User's Chat, and the Group where it was learned.
const SCOPES: MemoryScope[] = ["owner", "user", "group"];

// What the page holds while the User works on it. A draft only clears when
// another Bot is opened, so a refresh never takes what the User typed.
interface Draft {
  name: string;
  // The Model the Bot uses; empty means the Space's default.
  model: string;
  thinking: string;
  grant: string;
  // The Computer a move would send the Bot to.
  target: string;
  effect: Rule["effect"];
  tool: string;
  command: string;
  path: string;
  // The destructive action the User armed with a first press.
  armedReset: boolean;
  armedMove: boolean;
  // The risk is on screen and waits for the User's answer.
  armedFull: boolean;
  // What the full access switch shows while its write is on its way, or null
  // when it shows what the Space holds.
  fullWanted: boolean | null;
}

// The Model the Space uses by default, when the catalog names one.
function defaultModel(models: Model[]): Model | undefined {
  return models.find((model) => model.is_default);
}

// The thinking levels a Model accepts: its own, or every level when the catalog
// names none. A Model that cannot think accepts none.
function accepted(model: Model | undefined): ThinkingLevel[] {
  if (!model?.thinks) return [];
  return model.thinking_levels.length === 0 ? LEVELS : model.thinking_levels;
}

// The level a Model accepts that sits closest to the one the Bot already holds,
// so a Model that thinks less keeps working near where the User left it. A level
// the Model accepts outright stays as it is.
function nearestLevel(levels: ThinkingLevel[], want: string): ThinkingLevel | undefined {
  const held = levels.find((level) => level === want);
  if (held !== undefined) return held;
  const at = LEVELS.findIndex((level) => level === want);
  if (at < 0) return levels[0];
  let best: ThinkingLevel | undefined;
  let gap = Number.POSITIVE_INFINITY;
  for (const level of levels) {
    const next = Math.abs(LEVELS.indexOf(level) - at);
    if (next < gap) {
      best = level;
      gap = next;
    }
  }
  return best;
}

// The Model the Bot works with: its own, else the Space's default.
function inUse(models: Model[], model: string): Model | undefined {
  if (model !== "") return models.find((known) => known.id === model);
  return defaultModel(models);
}

// Who may use a Computer, in the User's words.
function sharingOf(computer: Computer): string {
  return computer.sharing === "exclusive"
    ? t("computer.sharing.exclusive")
    : t("computer.sharing.shared");
}

// Two Computers can share a default name. Only then does the target carry its
// full id, which is unique; a unique name stays as it is.
function targetLabel(computer: Computer, targets: Computer[]): string {
  const sameName = targets.filter((known) => known.name === computer.name).length;
  return sameName > 1 ? `${computer.name} · ${computer.id}` : computer.name;
}

function draftOf(bot: Bot | null): Draft {
  const model = bot?.model ?? "";
  // A Bot that names no level follows the Space's, so the draft holds none too:
  // filling it in here would name a level the Bot never chose.
  const thinking = bot?.thinking ?? "";
  return {
    name: bot?.name ?? "",
    model,
    thinking,
    grant: "",
    target: "",
    effect: "ask",
    tool: "",
    command: "",
    path: "",
    armedReset: false,
    armedMove: false,
    armedFull: false,
    // What the full access switch shows while its write is on its way, or null
    // when it shows what the Space holds.
    fullWanted: null as boolean | null,
  };
}

interface BotPageProps {
  bot: Bot | null;
  computer: Computer | null;
  // The Computers this Space knows, which is where a Bot may move.
  computers: Computer[];
  models: Model[];
  rules: Rule[];
  // The entries the Bot keeps, and the room each kind has left.
  memories: MemoryList;
  // A write is on its way to the Space.
  busy: boolean;
  error: string | null;
  onBack: () => void;
  onRename: (name: string) => void | Promise<void>;
  onModel: (model: string | null, thinking: ThinkingLevel | null) => void | Promise<void>;
  onGrantAdd: (path: string) => void | Promise<void>;
  onGrantRemove: (path: string) => void | Promise<void>;
  onFullAccess: (on: boolean) => void | Promise<void>;
  onMove: (computer: string) => void | Promise<void>;
  onReset: () => void | Promise<void>;
  onRuleAdd: (rule: {
    effect: Rule["effect"];
    tool: string;
    command_prefix: string | null;
    path_prefix: string | null;
  }) => void | Promise<void>;
  onRuleRemove: (rule: string) => void | Promise<void>;
  onMemoryRemove: (memory: string) => void | Promise<void>;
}

export default function BotPage(props: BotPageProps) {
  // The page opens on what the Space says about this Bot. Reading it untracked
  // marks it a one-time snapshot: the effect below is what follows the Bot.
  const [draft, setDraft] = createSignal(untrack(() => draftOf(props.bot)));
  // Another Bot was opened, so the page starts from what the Space says. Only
  // the Bot's own id moves the draft: a Bot the Space read again keeps what the
  // User has typed since.
  let shown = untrack(() => props.bot?.id ?? null);
  createTrackedEffect(() => {
    const id = props.bot?.id ?? null;
    if (id === shown) return;
    shown = id;
    setDraft(draftOf(props.bot));
  });

  const patch = (write: Partial<Draft>) => setDraft((current) => ({ ...current, ...write }));
  // A Hosted Computer works only in its own Workspace, so it takes no grant and
  // no full access.
  const hosted = () => props.computer?.kind !== "local";
  // What this Computer's daemon allows. A Space that names no capability leaves
  // the control off: an unnamed capability is one the page does not offer, so a
  // Windows Computer never opens a grant it would refuse.
  const grantsOpen = () => (props.computer?.supports_grants ?? false) === true;
  const fullOpen = () => (props.computer?.supports_full_access ?? false) === true;
  // The switch shows the Bot's own state, or what the User just asked for while
  // that write is on its way: a switch that snapped back would say the press did
  // nothing. The Space's answer puts it back on what the Space holds.
  const fullOn = () => draft().fullWanted ?? props.bot?.full_access ?? false;
  const renamed = () => {
    const name = draft().name.trim();
    return name !== "" && name !== (props.bot?.name ?? "");
  };
  // A Bot that follows the Space's default holds nothing of its own, which the
  // draft spells "": the two are the same choice, read the same way.
  const moved = () => draft().model !== (props.bot?.model ?? "");
  const reThought = () => draft().thinking !== (props.bot?.thinking ?? "");
  const ruleReady = () => draft().tool !== "";
  // The other Computers this User owns, which is where a Bot may move. A move
  // works between Hosted and Local Computers in either direction, so the list
  // names every one of them.
  const targets = () => props.computers.filter((known) => known.id !== props.computer?.id);
  const chosen = () => inUse(props.models, draft().model);
  const levels = () => accepted(chosen());

  function computerKind(): string {
    if (props.computer === null) return t("common.loading");
    return props.computer.kind === "local" ? t("chat.localComputer") : t("chat.hostedComputer");
  }

  // A Model the catalog names no level for keeps the one the Bot already has.
  function pickModel(model: string) {
    // The level stays one the new Model accepts: the nearest one it offers to
    // what the page holds, or its cheapest when the page holds none. A Bot that
    // follows the Space's default keeps following it, so choosing a Model never
    // names a level the Bot did not choose.
    const levels = accepted(inUse(props.models, model));
    const held = draft().thinking;
    patch({
      model,
      thinking: held === "" ? "" : (nearestLevel(levels, held) ?? ""),
      armedMove: false,
    });
  }

  async function saveName() {
    if (draft().name.trim() === "" || props.busy) return;
    await props.onRename(draft().name.trim());
  }

  async function saveModel() {
    if (props.busy || (!moved() && !reThought())) return;
    // An empty choice means the Space's default, which the Space holds as a
    // field of its own rather than as an absent Model or level.
    const model = draft().model === "" ? null : draft().model;
    const thinking = draft().thinking === "" ? null : (draft().thinking as ThinkingLevel);
    await props.onModel(model, thinking);
  }

  async function grant() {
    const path = draft().grant.trim();
    if (path === "" || props.busy) return;
    await props.onGrantAdd(path);
    patch({ grant: "" });
  }

  // Widening access asks for the owner's answer, so the switch only shows the
  // risk; taking it away needs no answer.
  async function fullAccess(on: boolean) {
    if (props.busy) return;
    patch({ armedFull: false, fullWanted: on });
    await props.onFullAccess(on);
    // The Space's word is what the switch shows again, so a write it refused
    // leaves the switch where the Bot really is.
    patch({ fullWanted: null });
  }

  async function reset() {
    if (props.busy) return;
    if (!draft().armedReset) {
      patch({ armedReset: true });
      return;
    }
    patch({ armedReset: false });
    await props.onReset();
  }

  async function move() {
    if (props.busy) return;
    const target = targets().find((known) => known.id === draft().target);
    if (!target) return;
    if (!draft().armedMove) {
      patch({ armedMove: true });
      return;
    }
    patch({ armedMove: false, target: "" });
    await props.onMove(target.id);
  }

  async function addRule() {
    if (!ruleReady() || props.busy) return;
    await props.onRuleAdd({
      effect: draft().effect,
      tool: draft().tool.trim(),
      command_prefix: draft().command.trim() === "" ? null : draft().command.trim(),
      path_prefix: draft().path.trim() === "" ? null : draft().path.trim(),
    });
    patch({ tool: "", command: "", path: "" });
  }

  // The name behind the identifier a rule stores: the Space's read adds it,
  // and a rule whose skill or server left its library falls back to the
  // identifier rather than showing a name the page made up.
  function skillName(rule: Rule): string {
    return rule.skill_name ?? rule.skill ?? "";
  }

  // What one rule covers, in the record's words. A rule that names a call the
  // Space answers says which skill or server it covers, so two rules for the
  // same tool are told apart.
  function scope(rule: Rule): string {
    const parts: string[] = [];
    if (rule.skill) parts.push(t("bot.rules.scope.skill", { skill: skillName(rule) }));
    if (rule.script) parts.push(t("bot.rules.scope.script", { script: rule.script }));
    if (rule.server) {
      parts.push(
        t("bot.rules.scope.server", {
          server: rule.server_name ?? rule.server,
          tool: rule.tool,
        }),
      );
    }
    if (rule.command_prefix)
      parts.push(t("chat.approval.scope.shell", { command: rule.command_prefix }));
    if (rule.path_prefix) parts.push(t("chat.approval.scope.files", { path: rule.path_prefix }));
    return parts.length === 0 ? t("bot.rules.scope.all", { tool: rule.tool }) : parts.join(" · ");
  }

  // What a rule's Remove control names. A rule that covers one skill's script
  // or one server's tool says which one, so its control is not one of two
  // alike.
  function removeLabel(rule: Rule): string {
    if (rule.skill) {
      return rule.script === undefined || rule.script === null
        ? t("bot.rules.remove", { tool: skillName(rule) })
        : t("bot.rules.remove.skill", { skill: skillName(rule), script: rule.script });
    }
    if (rule.server) {
      return t("bot.rules.remove.server", {
        server: rule.server_name ?? rule.server,
        tool: rule.tool,
      });
    }
    // A command or path rule is named by what it covers, not only by its tool:
    // two shell rules for different commands are two different rules.
    if (rule.command_prefix || rule.path_prefix)
      return t("bot.rules.remove", { tool: scope(rule) });
    return t("bot.rules.remove", { tool: rule.tool });
  }

  // The entries of one kind and scope, and the bytes that kind holds against
  // its limit: the limit belongs to the kind, not to a scope inside it.
  const scopedOf = (kind: MemoryKind, scope: MemoryScope) =>
    props.memories.entries.filter((entry) => entry.kind === kind && entry.scope === scope);
  const usedOf = (kind: MemoryKind) => props.memories.used[kind] ?? 0;
  const limitOf = (kind: MemoryKind) => props.memories.limits[kind] ?? 0;

  // Who wrote one entry: the Bot for a note, and the owner or another User
  // whose words gave a rule.
  function authorOf(entry: MemoryEntry): string {
    if (entry.author_bot) return t("bot.memory.author.bot");
    if (entry.author === props.bot?.owner) return t("bot.memory.author.owner");
    return t("bot.memory.author.user");
  }

  // What wrote one entry: the Message, run, or command the record names.
  function sourceOf(entry: MemoryEntry): string {
    return t(`bot.memory.source.${entry.source_kind}`, { source: entry.source });
  }

  return (
    <section class="page" aria-label={t("rail.bots")}>
      <header class="page__head">
        <button class="page__back" type="button" onClick={props.onBack} aria-label={t("bot.back")}>
          <Icon name="chevron-left" size="sm" />
        </button>
        <Show when={props.bot}>
          {(shown) => (
            <>
              <Avatar name={shown().name} />
              <div class="page__titles">
                <h1>{shown().name}</h1>
                <p class="page__sub">
                  <span>{computerKind()}</span>
                  <Show when={props.computer}>
                    {(computer) => (
                      <>
                        <span class="chat__dot" />
                        <span
                          class={computer().online ? "state-text state-text--online" : "state-text"}
                        >
                          {computer().online ? t("chat.online") : t("chat.offline")}
                        </span>
                      </>
                    )}
                  </Show>
                </p>
              </div>
            </>
          )}
        </Show>
      </header>

      <Show when={props.error}>
        <div class="banner banner--error page__error" role="alert">
          <Icon name="alert" size="sm" />
          {props.error}
        </div>
      </Show>

      <Show
        when={props.bot}
        fallback={
          <p class="page__empty" role="status">
            {t("bot.loading")}
          </p>
        }
      >
        {(shown) => (
          <div class="page__body">
            <form
              class="panel"
              onSubmit={(event) => {
                event.preventDefault();
                void saveName();
              }}
            >
              <div class="panel__head">
                <h2>{t("bot.section.name")}</h2>
              </div>
              <div class="panel__body">
                <label class="field">
                  <span class="field__label">{t("setup.bot.name")}</span>
                  <input
                    class="input"
                    name="bot_name"
                    maxlength={64}
                    value={draft().name}
                    onInput={(event) => patch({ name: event.currentTarget.value })}
                  />
                  <span class="field__hint">{t("setup.bot.nameHint")}</span>
                  <Show when={draft().name.trim() === ""}>
                    <span class="field__hint field__hint--warn">{t("bot.name.required")}</span>
                  </Show>
                </label>
              </div>
              <div class="panel__foot panel__foot--end">
                <button class="btn btn--primary" type="submit" disabled={props.busy || !renamed()}>
                  {props.busy ? t("bot.name.saving") : t("bot.name.save")}
                </button>
              </div>
            </form>

            <section class="panel">
              <div class="panel__head">
                <h2>{t("bot.section.computer")}</h2>
              </div>
              <div class="panel__body">
                <div class="facts">
                  <div class="fact">
                    <span class="fact__label">{t("bot.computer.name")}</span>
                    <span class="fact__value">{props.computer?.name ?? t("common.loading")}</span>
                  </div>
                  <div class="fact">
                    <span class="fact__label">{t("bot.computer.kind")}</span>
                    <span class="fact__value">{computerKind()}</span>
                  </div>
                  <div class="fact">
                    <span class="fact__label">{t("bot.computer.sharing")}</span>
                    <span class="fact__value">
                      {props.computer === null ? t("common.loading") : sharingOf(props.computer)}
                    </span>
                  </div>
                  <div class="fact">
                    <span class="fact__label">{t("chat.online")}</span>
                    <span class="fact__value">
                      {props.computer === null
                        ? t("common.loading")
                        : props.computer.online
                          ? t("chat.online")
                          : t("chat.offline")}
                    </span>
                  </div>
                  <div class="fact">
                    <span class="fact__label">{t("bot.computer.workspace")}</span>
                    <span class="fact__value mono">
                      {props.computer?.workspace ?? t("common.loading")}
                    </span>
                  </div>
                </div>

                <Show when={props.computer?.space}>
                  <div class="chips">
                    <span class="chip chip--default">{t("computer.space")}</span>
                  </div>
                </Show>

                <Show when={hosted()}>
                  <p class="panel__note">{t("bot.computer.hostedNote")}</p>
                </Show>

                <div class="field">
                  <span class="field__label">{t("bot.computer.move.choose")}</span>
                  <div class="field-row">
                    <select
                      class="input"
                      name="move_target"
                      disabled={props.busy || targets().length === 0}
                      value={draft().target}
                      onChange={(event) =>
                        patch({ target: event.currentTarget.value, armedMove: false })
                      }
                    >
                      <option value="">{t("bot.computer.move.choose")}</option>
                      <For each={targets()}>
                        {(target) => (
                          <option value={target.id}>{targetLabel(target, targets())}</option>
                        )}
                      </For>
                    </select>
                    <button
                      class={`btn ${draft().armedMove ? "btn--danger" : "btn--quiet"}`}
                      type="button"
                      disabled={props.busy || draft().target === ""}
                      onClick={() => void move()}
                    >
                      <Icon name="repeat" size="sm" />
                      {draft().armedMove ? t("bot.computer.move.armed") : t("bot.computer.move")}
                    </button>
                  </div>
                  <Show
                    when={targets().length > 0}
                    fallback={<span class="field__hint">{t("bot.computer.move.none")}</span>}
                  >
                    <span class="field__hint">{t("bot.computer.move.hint")}</span>
                  </Show>
                </div>
              </div>
              <Show when={props.computer?.kind === "hosted"}>
                <div class="panel__foot">
                  <p class="panel__note">{t("bot.computer.reset.hint")}</p>
                  <button
                    class={`btn ${draft().armedReset ? "btn--danger" : "btn--quiet"}`}
                    type="button"
                    disabled={props.busy}
                    onClick={() => void reset()}
                  >
                    <Icon name="repeat" size="sm" />
                    {draft().armedReset ? t("bot.computer.reset.armed") : t("bot.computer.reset")}
                  </button>
                </div>
              </Show>
              <Show when={draft().armedMove}>
                <p class="panel__note" role="status">
                  {t("bot.computer.move.armed")}
                </p>
              </Show>
            </section>

            <section class="panel">
              <div class="panel__head">
                <h2>{t("bot.section.model")}</h2>
              </div>
              <div class="panel__body">
                <label class="field">
                  <span class="field__label">{t("bot.section.model")}</span>
                  <select
                    class="input"
                    name="bot_model"
                    disabled={props.busy || props.models.length === 0}
                    value={draft().model}
                    onChange={(event) => pickModel(event.currentTarget.value)}
                  >
                    <option value="">{t("bot.model.default")}</option>
                    <For each={props.models}>
                      {(model) => (
                        <option value={model.id} disabled={!model.tools}>
                          {model.name}
                          {model.tools ? "" : ` · ${t("bot.model.noTools")}`}
                        </option>
                      )}
                    </For>
                  </select>
                  <span class="field__hint">{t("bot.model.hint")}</span>
                  <Show when={props.models.length === 0}>
                    <span class="field__hint field__hint--warn">{t("bot.model.empty")}</span>
                  </Show>
                </label>

                <label class="field">
                  <span class="field__label">{t("bot.model.thinking")}</span>
                  <select
                    class="input"
                    name="bot_thinking"
                    disabled={props.busy || levels().length === 0}
                    value={draft().thinking}
                    onChange={(event) => patch({ thinking: event.currentTarget.value })}
                  >
                    {/* A Bot that names no level follows the Space's, which is
                        a choice of its own: this is how it goes back to it. */}
                    <option value="">{t("bot.model.default")}</option>
                    <For each={levels()}>
                      {(level) => <option value={level}>{t(`level.${level}`)}</option>}
                    </For>
                  </select>
                  <Show
                    when={levels().length > 0}
                    fallback={<span class="field__hint">{t("bot.model.thinking.none")}</span>}
                  >
                    <Show when={shown().thinking == null}>
                      <span class="field__hint">{t("bot.model.default")}</span>
                    </Show>
                  </Show>
                </label>

                <Show when={chosen()}>
                  {(model) => (
                    <div class="chips">
                      <span class="chip">
                        {t("bot.model.context", { limit: model().context_limit })}
                      </span>
                      <span class="chip">
                        {t("bot.model.output", { limit: model().max_output })}
                      </span>
                      <span class="chip">
                        {model().reads_images
                          ? t("bot.model.readsImages")
                          : t("bot.model.noImages")}
                      </span>
                      <span class="chip">
                        {model().tools ? t("bot.model.callsTools") : t("bot.model.noTools")}
                      </span>
                    </div>
                  )}
                </Show>
              </div>
              <div class="panel__foot panel__foot--end">
                <button
                  class="btn btn--primary"
                  type="button"
                  disabled={props.busy || (!moved() && !reThought())}
                  onClick={() => void saveModel()}
                >
                  {props.busy ? t("bot.model.saving") : t("bot.model.save")}
                </button>
              </div>
            </section>

            <section class="panel">
              <div class="panel__head">
                <h2>{t("bot.section.access")}</h2>
              </div>
              <div class="panel__body">
                <div class="access-row">
                  <span class="access-row__icon">
                    <Icon name="lock" size="sm" />
                  </span>
                  <span>
                    <b>{t("bot.access.ownerOnly")}</b>
                    <span>{t("setup.bot.accessHint")}</span>
                  </span>
                </div>
                <p class="panel__note">{t("bot.access.gap")}</p>
              </div>
            </section>

            <section class="panel">
              <div class="panel__head">
                <h2>{t("bot.section.folders")}</h2>
              </div>
              <div class="panel__body">
                <Show
                  when={!hosted()}
                  fallback={<p class="panel__note">{t("bot.folders.hosted")}</p>}
                >
                  {/* A grant the Bot holds can always be taken away: the Space
                      reads a removal as a narrowing and allows it, whichever way
                      this Computer answers. Only the offer of a new one goes. */}
                  <Show
                    when={shown().grants.length > 0}
                    fallback={<p class="rows__empty">{t("bot.folders.empty")}</p>}
                  >
                    <div class="rows">
                      <For each={shown().grants}>
                        {(path) => (
                          <div class="row">
                            <Icon name="folder" size="sm" />
                            <span class="row__text mono">{path}</span>
                            <button
                              class="row__remove"
                              type="button"
                              aria-label={t("bot.folders.remove", { path })}
                              title={t("bot.folders.remove", { path })}
                              disabled={props.busy}
                              onClick={() => void props.onGrantRemove(path)}
                            >
                              <Icon name="x" size="sm" />
                            </button>
                          </div>
                        )}
                      </For>
                    </div>
                  </Show>
                  {/* Only a new grant goes: a Computer whose daemon takes no
                        grant says so instead of offering a folder it would
                        refuse. */}
                  <Show
                    when={grantsOpen()}
                    fallback={<p class="panel__note">{t("bot.access.machine")}</p>}
                  >
                    <form
                      class="field"
                      onSubmit={(event) => {
                        event.preventDefault();
                        void grant();
                      }}
                    >
                      <span class="field__label">{t("bot.folders.add")}</span>
                      <div class="field-row">
                        <input
                          class="input mono"
                          name="grant_path"
                          placeholder={t("bot.folders.placeholder")}
                          value={draft().grant}
                          onInput={(event) => patch({ grant: event.currentTarget.value })}
                        />
                        <button
                          class="btn btn--quiet"
                          type="submit"
                          disabled={props.busy || draft().grant.trim() === ""}
                        >
                          {props.busy ? t("bot.folders.adding") : t("common.add")}
                        </button>
                      </div>
                      <span class="field__hint">{t("bot.folders.hint")}</span>
                    </form>
                  </Show>
                </Show>
              </div>
            </section>

            <section class="panel">
              <div class="panel__head">
                <h2>{t("bot.section.fullAccess")}</h2>
              </div>
              <div class="panel__body">
                <Show
                  when={!hosted()}
                  fallback={<p class="panel__note">{t("bot.fullAccess.hosted")}</p>}
                >
                  {/* Full access that is on can always be turned off; only
                      turning it on needs a Computer whose daemon allows it. */}
                  <Show
                    when={fullOpen() || shown().full_access}
                    fallback={<p class="panel__note">{t("bot.access.machine")}</p>}
                  >
                    <label class="check">
                      <input
                        type="checkbox"
                        name="full_access"
                        checked={fullOn()}
                        disabled={props.busy}
                        onChange={(event) => {
                          // Turning it on asks first, and only where this
                          // Computer's daemon allows it. Turning it off never
                          // asks: what is on can always go off.
                          if (event.currentTarget.checked) {
                            if (fullOpen()) patch({ armedFull: true, fullWanted: true });
                          } else {
                            void fullAccess(false);
                          }
                        }}
                      />
                      <span>{t("bot.fullAccess.label")}</span>
                    </label>
                    <Show when={shown().full_access}>
                      <p class="panel__note" role="status">
                        {t("bot.fullAccess.on")}
                      </p>
                    </Show>
                    <Show when={draft().armedFull}>
                      <div class="risk">
                        <p class="risk__text">{t("bot.fullAccess.risk")}</p>
                        <div class="risk__actions">
                          <button
                            class="btn btn--primary"
                            type="button"
                            disabled={props.busy}
                            onClick={() => void fullAccess(true)}
                          >
                            {t("bot.fullAccess.confirm")}
                          </button>
                          <button
                            class="btn btn--quiet"
                            type="button"
                            onClick={() => patch({ armedFull: false, fullWanted: null })}
                          >
                            {t("common.cancel")}
                          </button>
                        </div>
                      </div>
                    </Show>
                  </Show>
                </Show>
              </div>
            </section>

            <section class="panel">
              <div class="panel__head">
                <h2>{t("bot.section.rules")}</h2>
              </div>
              <div class="panel__body">
                <Show
                  when={props.rules.length > 0}
                  fallback={<p class="rows__empty">{t("bot.rules.empty")}</p>}
                >
                  <div class="rows">
                    <For each={props.rules}>
                      {(rule) => (
                        <div class="row">
                          <span class={`chip chip--${rule.effect}`}>
                            {t(`bot.rules.effect.${rule.effect}`)}
                          </span>
                          <span class="row__text">
                            <span class="row__name mono">{rule.tool}</span>
                            <span class="row__meta">{scope(rule)}</span>
                          </span>
                          <button
                            class="row__remove"
                            type="button"
                            aria-label={removeLabel(rule)}
                            title={removeLabel(rule)}
                            disabled={props.busy}
                            onClick={() => void props.onRuleRemove(rule.rule)}
                          >
                            <Icon name="x" size="sm" />
                          </button>
                        </div>
                      )}
                    </For>
                  </div>
                </Show>

                {/* The form adds rules for actions the Bot's Computer runs, and
                    a Hosted Computer takes none of those: it says why instead.
                    A rule for a call the Space answers still applies on a
                    Hosted Bot, and the rows above list and remove it. */}
                <Show
                  when={hosted()}
                  fallback={
                    <form
                      class="panel__form"
                      onSubmit={(event) => {
                        event.preventDefault();
                        void addRule();
                      }}
                    >
                      <div class="field-row">
                        <label class="field">
                          <span class="field__label">{t("bot.rules.effect")}</span>
                          <select
                            class="input"
                            name="rule_effect"
                            value={draft().effect}
                            onChange={(event) =>
                              patch({ effect: event.currentTarget.value as Rule["effect"] })
                            }
                          >
                            <option value="ask">{t("bot.rules.effect.ask")}</option>
                            <option value="allow">{t("bot.rules.effect.allow")}</option>
                          </select>
                        </label>
                        <label class="field">
                          <span class="field__label">{t("bot.rules.tool")}</span>
                          <select
                            class="input mono"
                            name="rule_tool"
                            value={draft().tool}
                            onChange={(event) => patch({ tool: event.currentTarget.value })}
                          >
                            <option value="">{t("bot.rules.tool.choose")}</option>
                            <For each={TOOLS}>{(tool) => <option value={tool}>{tool}</option>}</For>
                          </select>
                        </label>
                      </div>
                      <div class="field-row">
                        <label class="field">
                          <span class="field__label">{t("bot.rules.commandPrefix")}</span>
                          <input
                            class="input mono"
                            name="rule_command"
                            placeholder="git"
                            value={draft().command}
                            onInput={(event) => patch({ command: event.currentTarget.value })}
                          />
                        </label>
                        <label class="field">
                          <span class="field__label">{t("bot.rules.pathPrefix")}</span>
                          <input
                            class="input mono"
                            name="rule_path"
                            placeholder="/data"
                            value={draft().path}
                            onInput={(event) => patch({ path: event.currentTarget.value })}
                          />
                        </label>
                      </div>
                      <div class="panel__foot panel__foot--end">
                        <button
                          class="btn btn--primary"
                          type="submit"
                          disabled={props.busy || !ruleReady()}
                        >
                          {props.busy ? t("bot.rules.adding") : t("bot.rules.add")}
                        </button>
                      </div>
                      <span class="field__hint">{t("bot.rules.hint")}</span>
                    </form>
                  }
                >
                  <p class="panel__note">{t("bot.rules.hosted")}</p>
                </Show>
              </div>
            </section>

            <section class="panel">
              <div class="panel__head">
                <h2>{t("bot.section.memory")}</h2>
              </div>
              <div class="panel__body">
                {/* An empty memory still shows how much each kind may hold, so
                    the page names the room before the Bot keeps anything. */}
                <Show when={props.memories.entries.length === 0}>
                  <p class="rows__empty">{t("bot.memory.empty")}</p>
                </Show>
                <div class="memory">
                  <For each={KINDS}>
                    {(kind) => (
                      <div class="memory__group">
                        <div class="memory__head">
                          <h3 class="memory__kind">{t(`bot.memory.${kind}s`)}</h3>
                          <span class="memory__capacity">
                            {t("bot.memory.capacity", {
                              used: formatBytes(usedOf(kind)),
                              limit: formatBytes(limitOf(kind)),
                            })}
                          </span>
                        </div>
                        {/* One group per scope the kind holds entries in;
                              a scope with none stays off the page. */}
                        <For each={SCOPES}>
                          {(scope) => (
                            <Show when={scopedOf(kind, scope).length > 0}>
                              <div class="memory__scope">
                                <h4 class="memory__scope-title">
                                  {`${t(`bot.memory.${kind}s`)} · ${t(`bot.memory.scope.${scope}`)}`}
                                </h4>
                                <div class="rows">
                                  <For each={scopedOf(kind, scope)}>
                                    {(entry) => (
                                      <div class="row">
                                        <span class="chip">
                                          {t(`bot.memory.scope.${entry.scope}`)}
                                        </span>
                                        <span class="row__text">
                                          {/* A User who does not read an entry
                                                sees its metadata only, the way
                                                the Space answers it. */}
                                          <Show when={entry.readable}>
                                            <span class="row__name memory__text">{entry.text}</span>
                                          </Show>
                                          <span class="row__meta">
                                            {`${authorOf(entry)} · ${sourceOf(entry)} · ${formatBytes(entry.size)}`}
                                          </span>
                                        </span>
                                        <button
                                          class="row__remove"
                                          type="button"
                                          aria-label={t("bot.memory.remove")}
                                          title={t("bot.memory.remove")}
                                          disabled={props.busy}
                                          onClick={() => void props.onMemoryRemove(entry.memory)}
                                        >
                                          <Icon name="x" size="sm" />
                                        </button>
                                      </div>
                                    )}
                                  </For>
                                </div>
                              </div>
                            </Show>
                          )}
                        </For>
                      </div>
                    )}
                  </For>
                </div>
              </div>
            </section>
          </div>
        )}
      </Show>
    </section>
  );
}
