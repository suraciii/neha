import { For, Show, createSignal } from "solid-js";

import { t } from "../i18n";
import type { Bot, InstalledPackage, Skill } from "../types";
import Icon from "./Icon";

export interface SkillsViewProps {
  account: { id: string; displayName: string; isAdmin: boolean };
  bots: Bot[];
  skills: Skill[];
  // The packages the Admin installed, when this page could read them, so a
  // skill a package brought can name it.
  packages?: InstalledPackage[];
  busy: boolean;
  error: string | null;
  onAdd: (input: {
    name: string;
    description: string;
    body: string;
    bot: string | null;
    scripts: { name: string; content: string }[];
  }) => Promise<void>;
  onRemove: (skill: string) => Promise<void>;
  onBot: (bot: string, skill: string, enabled: boolean) => Promise<void>;
}

interface ScriptDraft {
  name: string;
  content: string;
}

// Runs one command the page owns. A refusal is reported through `error`, so the
// view does not repeat it.
function run(action: () => Promise<void>) {
  void action().catch(() => undefined);
}

// The Skills view: the Space's library and every Bot's own, with the controls
// the Space would accept from this caller. Adding a skill joins one library;
// a skill a package brought names the package, and its scripts still run only
// as the Bot's actions under an approval.
export default function SkillsView(props: SkillsViewProps) {
  const [name, setName] = createSignal("");
  const [description, setDescription] = createSignal("");
  const [body, setBody] = createSignal("");
  // The library the Add form writes to: the Space's when the reader may add
  // there, else the first Bot's.
  const [library, setLibrary] = createSignal(props.account.isAdmin ? "space" : (props.bots[0]?.id ?? ""));
  const [scripts, setScripts] = createSignal<ScriptDraft[]>([]);

  // The Space's library only takes the Admin's additions; every Bot's library
  // takes its owner's, and the Space lists the reader only the Bots it owns.
  const chosenLibrary = (): string => {
    const chosen = library();
    if (chosen === "space") return props.account.isAdmin ? "space" : (props.bots[0]?.id ?? "");
    return props.bots.some((bot) => bot.id === chosen) ? chosen : (props.bots[0]?.id ?? "");
  };
  const canAdd = () => props.account.isAdmin || props.bots.length > 0;
  const ready = () => name().trim() !== "" && description().trim() !== "" && chosenLibrary() !== "";

  // The list is still on its way, so the view says it is loading instead of
  // saying it is empty.
  const loading = (): boolean => props.busy && props.skills.length === 0;

  // The skills one library holds, in the order the Space lists them.
  const spaceSkills = () => props.skills.filter((skill) => skill.library === "space");
  const botSkills = (bot: Bot) =>
    props.skills.filter((skill) => skill.library === "bot" && skill.bot === bot.id);

  // The package a skill came from, when this page may read the package list.
  function packageName(skill: Skill): string | null {
    if (!skill.plugin) return null;
    return props.packages?.find((pkg) => pkg.plugin === skill.plugin)?.name ?? null;
  }

  // Only the Admin removes from the Space's library; a Bot's library is its
  // owner's, and every Bot this page lists belongs to the reader.
  function removable(skill: Skill): boolean {
    return skill.library === "space" ? props.account.isAdmin : true;
  }

  // The Bots a skill can be turned on for: every one for the Space's skills,
  // its own owner for a Bot's own skill.
  function enabledBots(skill: Skill): Bot[] {
    if (skill.library === "space") return props.bots;
    return props.bots.filter((bot) => bot.id === skill.bot);
  }

  function addScript() {
    setScripts((current) => [...current, { name: "", content: "" }]);
  }

  function updateScript(index: number, field: keyof ScriptDraft, value: string) {
    setScripts((current) =>
      current.map((script, position) => (position === index ? { ...script, [field]: value } : script)),
    );
  }

  function removeScript(index: number) {
    setScripts((current) => current.filter((_, position) => position !== index));
  }

  function addSkill(event: SubmitEvent) {
    event.preventDefault();
    if (props.busy || !ready()) return;
    const chosen = chosenLibrary();
    const carried = scripts()
      .filter((script) => script.name.trim() !== "")
      .map((script) => ({ name: script.name.trim(), content: script.content }));
    run(() =>
      props.onAdd({
        name: name().trim(),
        description: description().trim(),
        body: body(),
        bot: chosen === "space" ? null : chosen,
        scripts: carried,
      }),
    );
  }

  function skillRow(skill: Skill) {
    // A getter, so the badge appears when the package list arrives after the
    // skill list does.
    const origin = () => packageName(skill);
    return (
      <div class="library__row" data-skill={skill.id}>
        <div class="library__server">
          <div class="library__name">
            <h3>{skill.name}</h3>
            <Show when={origin()}>
              {(label) => <span class="badge">{t("skills.fromPackage", { name: label() })}</span>}
            </Show>
          </div>
          <p class="library__description">{skill.description}</p>
          <div class="facts">
            <div class="fact">
              <span class="fact__label">{t("skills.scripts")}</span>
              <span class="fact__value">{skill.scripts.length}</span>
            </div>
            <Show when={skill.scripts.length > 0}>
              <div class="fact">
                <span class="fact__label">{t("skills.scriptFiles")}</span>
                <span class="fact__value mono">
                  {skill.scripts.map((script) => script.name).join(", ")}
                </span>
              </div>
            </Show>
          </div>
          <Show when={enabledBots(skill).length > 0}>
            <div class="chips" role="group" aria-label={t("skills.bots")}>
              <For each={enabledBots(skill)}>
                {(bot) => {
                  const uses = () => bot.skills?.includes(skill.id) === true;
                  return (
                    <label class="chip chip--button">
                      <input
                        type="checkbox"
                        checked={uses()}
                        disabled={props.busy}
                        aria-label={
                          uses()
                            ? t("skills.disableFor", { bot: bot.name })
                            : t("skills.enableFor", { bot: bot.name })
                        }
                        onChange={(event) =>
                          run(() => props.onBot(bot.id, skill.id, event.currentTarget.checked))
                        }
                      />
                      {bot.name}
                    </label>
                  );
                }}
              </For>
            </div>
          </Show>
        </div>
        <div class="library__actions">
          <Show when={removable(skill)}>
            <button
              class="btn btn--danger"
              type="button"
              disabled={props.busy}
              aria-label={t("skills.removeNamed", { name: skill.name })}
              onClick={() => run(() => props.onRemove(skill.id))}
            >
              {t("skills.remove")}
            </button>
          </Show>
        </div>
      </div>
    );
  }

  return (
    <div class="library">
      <h1>{t("skills.title")}</h1>

      <Show when={props.error}>
        <p class="banner banner--error" role="alert">
          <Icon name="alert" size="sm" />
          {props.error}
        </p>
      </Show>

      <Show
        when={canAdd()}
        fallback={<p class="muted">{t("skills.add.needBot")}</p>}
      >
        <form class="panel" data-form="skill-add" onSubmit={addSkill}>
          <div class="panel__head">
            <div>
              <h2>{t("skills.add.title")}</h2>
            </div>
          </div>
          <div class="panel__body">
            <div class="field-row">
              <label class="field">
                <span class="field__label">{t("skills.name")}</span>
                <input
                  class="input"
                  name="skill_name"
                  maxlength={64}
                  required
                  value={name()}
                  onInput={(event) => setName(event.currentTarget.value)}
                />
                <span class="field__hint">{t("skills.nameHint")}</span>
              </label>
              <label class="field">
                <span class="field__label">{t("skills.description")}</span>
                <input
                  class="input"
                  name="skill_description"
                  maxlength={1024}
                  required
                  value={description()}
                  onInput={(event) => setDescription(event.currentTarget.value)}
                />
              </label>
            </div>
            <label class="field">
              <span class="field__label">{t("skills.instructions")}</span>
              <textarea
                class="input library__textarea"
                name="skill_body"
                rows={6}
                value={body()}
                onInput={(event) => setBody(event.currentTarget.value)}
              />
            </label>
            <label class="field">
              <span class="field__label">{t("skills.library")}</span>
              <select
                class="input"
                name="skill_library"
                value={chosenLibrary()}
                onChange={(event) => setLibrary(event.currentTarget.value)}
              >
                <Show when={props.account.isAdmin}>
                  <option value="space">{t("skills.spaceOption")}</option>
                </Show>
                <For each={props.bots}>
                  {(bot) => <option value={bot.id}>{bot.name}</option>}
                </For>
              </select>
            </label>
            <div class="library__scripts">
              <span class="field__label">{t("skills.scripts")}</span>
              <For each={scripts()}>
                {(script, index) => (
                  <div class="library__script">
                    <label class="field">
                      <span class="field__label">{t("skills.script.name")}</span>
                      <input
                        class="input mono"
                        name={`script_name_${index()}`}
                        maxlength={255}
                        value={script.name}
                        onInput={(event) => updateScript(index(), "name", event.currentTarget.value)}
                      />
                    </label>
                    <label class="field">
                      <span class="field__label">{t("skills.script.content")}</span>
                      <textarea
                        class="input mono library__script__content"
                        name={`script_content_${index()}`}
                        rows={3}
                        value={script.content}
                        onInput={(event) => updateScript(index(), "content", event.currentTarget.value)}
                      />
                    </label>
                    <button
                      class="btn btn--quiet"
                      type="button"
                      aria-label={t("skills.script.remove")}
                      onClick={() => removeScript(index())}
                    >
                      <Icon name="minus" size="sm" />
                    </button>
                  </div>
                )}
              </For>
              <button class="link-btn" type="button" onClick={addScript}>
                <Icon name="plus" size="sm" />
                {t("skills.addScript")}
              </button>
            </div>
          </div>
          <div class="panel__foot">
            <span class="panel__note">{t("skills.add.scriptsNote")}</span>
            <button class="btn btn--primary" type="submit" disabled={props.busy || !ready()}>
              {t("skills.add")}
              <Icon name="plus" size="sm" />
            </button>
          </div>
        </form>
      </Show>

      <Show
        when={!loading()}
        fallback={<p class="muted library__loading">{t("library.loading")}</p>}
      >
        <section class="library__group" data-group="space" aria-label={t("skills.space")}>
          <div class="library__head">
            <h2>{t("skills.space")}</h2>
            <Show when={props.account.isAdmin}>
              <span class="badge">{t("setup.adminOnly")}</span>
            </Show>
          </div>
          <Show
            when={spaceSkills().length > 0}
            fallback={<p class="muted">{t("skills.empty.space")}</p>}
          >
            <For each={spaceSkills()}>{(skill) => skillRow(skill)}</For>
          </Show>
        </section>

        <For each={props.bots}>
          {(bot) => (
            <section
              class="library__group"
              data-group={bot.id}
              aria-label={t("skills.botLibrary", { name: bot.name })}
            >
              <div class="library__head">
                <h2>{t("skills.botLibrary", { name: bot.name })}</h2>
              </div>
              <Show
                when={botSkills(bot).length > 0}
                fallback={<p class="muted">{t("skills.empty.bot")}</p>}
              >
                <For each={botSkills(bot)}>{(skill) => skillRow(skill)}</For>
              </Show>
            </section>
          )}
        </For>
      </Show>
    </div>
  );
}
