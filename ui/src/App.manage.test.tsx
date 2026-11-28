import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { fireEvent, waitFor } from "@solidjs/testing-library";
import { flush } from "solid-js";
import { describe, expect, it, vi } from "vitest";

import { formatBytes } from "./lib/files";
import { ProtocolError } from "./lib/ws";
import { space } from "./test/fakeSpace";
import {
  button,
  drain,
  element,
  openChat,
  panelText,
  record,
  renderApp,
  useView,
} from "./test/view";
import type { MemoryEntry, Model, Rule } from "./types";

vi.mock("./lib/ws", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./lib/ws")>()),
  Commands: (await import("./test/fakeSpace")).FakeCommands,
}));

type View = ReturnType<typeof renderApp>;

// A Model that thinks before it answers, and one that cannot call tools.
const thinker: Model = {
  id: "m1",
  provider: "v1",
  name: "Atlas",
  context_limit: 400_000,
  max_output: 128_000,
  reads_images: true,
  thinks: true,
  tools: true,
  is_default: true,
  thinking_levels: ["minimal", "medium", "high"],
};

// A Model that answers but never thinks.
const plain: Model = {
  id: "m3",
  provider: "v1",
  name: "Atlas plain",
  context_limit: 200_000,
  max_output: 16_000,
  reads_images: false,
  thinks: false,
  tools: true,
  is_default: false,
  thinking_levels: [],
};

const quiet: Model = {
  id: "m2",
  provider: "v1",
  name: "Atlas mini",
  context_limit: 100_000,
  max_output: 8_000,
  reads_images: false,
  thinks: false,
  tools: false,
  is_default: false,
  thinking_levels: [],
};

// The field the test names, by its name.
function field<T extends HTMLElement>(view: View, name: string): T {
  return element<T>(view, `[name="${name}"]`);
}

// The value of one fact in a panel: the label says what the value is, so the
// test reads the answer rather than every word the panel holds.
function factValue(view: View, heading: string, label: string): string {
  const panel = Array.from(view.container.querySelectorAll(".panel")).find(
    (known) => known.querySelector("h2")?.textContent === heading,
  );
  if (!panel) throw new Error(`no ${heading} panel in the view`);
  const fact = Array.from(panel.querySelectorAll(".fact")).find(
    (known) => known.querySelector(".fact__label")?.textContent === label,
  );
  if (!fact) throw new Error(`no ${label} fact in the ${heading} panel`);
  return fact.querySelector(".fact__value")?.textContent ?? "";
}

// Types into a field, and lets the page read it.
function type(view: View, name: string, value: string) {
  flush();
  fireEvent.input(field<HTMLInputElement>(view, name), { target: { value } });
  flush();
}

// Chooses from a select, and lets the page read it. The drain before the change
// lets an answer that is still on its way land first, so the page reads the
// Space's word, not the one before it.
function choose(view: View, name: string, value: string) {
  flush();
  fireEvent.change(field<HTMLSelectElement>(view, name), { target: { value } });
  flush();
}

// Presses a button. The page answers it, and the test waits for the answer
// with `waitFor` when it reads the screen back.
function press(view: View, text: string) {
  fireEvent.click(button(view, text));
}

// Opens the Bots surface and the one Bot's page in it.
// `which` names the Bot in the sidebar list, so a test can start on a later one.
async function openBots(which = 0): Promise<View> {
  const view = renderApp();
  await waitFor(() => expect(element(view, ".rail__item:nth-of-type(3)")).toBeTruthy());
  flush(() => fireEvent.click(element(view, ".rail__item:nth-of-type(3)")));
  // The sidebar lists every Bot the Space knows.
  const bots = () => view.container.querySelectorAll(".conv");
  await waitFor(() => expect(bots().length).toBeGreaterThan(which));
  flush(() => fireEvent.click(bots()[which]!));
  await waitFor(() => expect(element(view, ".page__body")).toBeTruthy());
  return view;
}

// Opens the Settings surface on the section the test names: Providers, Models,
// Computers, or the catalog.
async function openSettings(section = "Providers"): Promise<View> {
  const view = renderApp();
  await waitFor(() => expect(element(view, ".rail__item:nth-of-type(4)")).toBeTruthy());
  flush(() => fireEvent.click(element(view, ".rail__item:nth-of-type(4)")));
  // The sidebar lists every section the Settings surface opens.
  await waitFor(() => expect(view.container.querySelectorAll(".conv")).toHaveLength(4));
  flush(() => fireEvent.click(button(view, section)));
  await waitFor(() => expect(element(view, ".page__body")).toBeTruthy());
  return view;
}

// Opens the second Bot from the sidebar, the way a User moves between Bots.
// Opens the Bot the test names, by the name its row shows. The list holds more
// than its Bot rows, so a row is found by what it says rather than by where it
// sits.
async function openBotNamed(view: View, name: string) {
  const row = () =>
    Array.from(view.container.querySelectorAll<HTMLButtonElement>(".conv")).find(
      (known) => known.querySelector(".conv__name")?.textContent === name,
    );
  if (!row()) throw new Error(`no ${name} row in the view`);
  flush(() => fireEvent.click(row()!));
  await waitFor(() => expect(element(view, ".page__titles h1").textContent).toBe(name));
}

async function openOtherBot(view: View) {
  await openBotNamed(view, "Quill");
}

// The rule the Bot holds, which a test reads back.
function askRule(): Rule {
  return {
    rule: "rule1",
    bot: "b1",
    user: "u1",
    seq: "1",
    effect: "ask",
    tool: "shell",
    command_prefix: "git",
    path_prefix: null,
  };
}

// One memory entry as memory_list names it. A test may set the entry's text,
// which decides its size, or the size alone for an entry the User does not
// read: the Space answers an unreadable entry with no text.
function memoryOf(overrides: Partial<MemoryEntry> = {}): MemoryEntry {
  const text = overrides.text ?? "";
  const entry: MemoryEntry = {
    memory: "mem1",
    kind: "rule",
    scope: "owner",
    scope_id: null,
    author: "u1",
    author_bot: false,
    source: "m1",
    source_kind: "message",
    size: new TextEncoder().encode(text).length,
    readable: true,
    text,
    ...overrides,
  };
  if (!entry.readable) delete entry.text;
  return entry;
}

// The Memory panel of the Bot page on screen.
function memoryPanel(view: View): HTMLElement {
  const panel = Array.from(view.container.querySelectorAll<HTMLElement>(".panel")).find(
    (known) => known.querySelector("h2")?.textContent === "Memory",
  );
  if (!panel) throw new Error("no Memory panel in the view");
  return panel;
}

// One row of the Memory panel, found by a word it holds, so a test reads the
// entry it set rather than whichever row the list shows first.
function memoryRow(view: View, text: string): HTMLElement {
  const row = Array.from(memoryPanel(view).querySelectorAll<HTMLElement>(".row")).find((known) =>
    known.textContent?.includes(text),
  );
  if (!row) throw new Error(`no memory row holds ${text}`);
  return row;
}

describe("App management", () => {
  useView();

  it("names a Bot again", async () => {
    const view = await openBots();
    expect(element(view, ".page__titles h1").textContent).toBe("Scout");

    type(view, "bot_name", "Nova");
    await waitFor(() => expect(button(view, "Save name").disabled).toBe(false));
    press(view, "Save name");

    await waitFor(() => expect(space.updates).toHaveLength(1));
    expect(space.updates[0]).toMatchObject({ bot: "b1", name: "Nova" });
    // The page and the sidebar both show the new name.
    await waitFor(() => expect(element(view, ".page__titles h1").textContent).toBe("Nova"));
    await waitFor(() => expect(element(view, ".conv__name").textContent).toBe("Nova"));
    // An empty name saves nothing.
    type(view, "bot_name", "");
    await waitFor(() => expect(button(view, "Save name").disabled).toBe(true));
  });

  it("saves the Model a Bot uses and how much it thinks", async () => {
    space.models = [thinker, quiet];
    const view = await openBots();

    // The Model lists what each one can do; one that cannot call tools cannot be
    // chosen.
    const models = field<HTMLSelectElement>(view, "bot_model");
    expect(models.options).toHaveLength(3);
    expect(models.options[1]?.value).toBe("m1");
    expect(models.options[2]?.disabled).toBe(true);
    expect(models.options[2]?.textContent).toContain("Cannot call tools");

    // A Model that thinks names the levels it accepts, beside the Space's own
    // default, which is where a Bot that names no level stays.
    choose(view, "bot_model", "m1");
    await waitFor(() =>
      expect(field<HTMLSelectElement>(view, "bot_thinking").options).toHaveLength(4),
    );
    expect(field<HTMLSelectElement>(view, "bot_thinking").options[0]?.textContent).toBe(
      "Space default",
    );
    choose(view, "bot_thinking", "medium");
    await waitFor(() => expect(panelText(view, "Model")).toContain("Reads images"));

    press(view, "Save Model");
    await waitFor(() => expect(space.updates).toHaveLength(1));
    expect(space.updates[0]).toMatchObject({ bot: "b1", model: "m1", thinking: "medium" });

    // A second save sends nothing: the Bot already holds this choice.
    await waitFor(() => expect(button(view, "Save Model").disabled).toBe(true));
  });

  it("keeps only a level the newly chosen Model accepts", async () => {
    space.models = [
      thinker,
      { ...quiet, thinks: true, tools: true, thinking_levels: ["minimal", "low", "medium"] },
    ];
    const view = await openBots();
    choose(view, "bot_model", "m1");
    choose(view, "bot_thinking", "high");
    press(view, "Save Model");
    await waitFor(() => expect(space.bot.thinking).toBe("high"));
    // The page reads the Bot again, so it stops offering to save what the Bot
    // already holds.
    await waitFor(() => expect(button(view, "Save Model").disabled).toBe(true));

    // The second Model thinks less: it accepts no "high", so the Bot keeps the
    // nearest level it does accept.
    choose(view, "bot_model", "m2");
    await waitFor(() =>
      expect(field<HTMLSelectElement>(view, "bot_thinking").value).toBe("medium"),
    );
    press(view, "Save Model");
    await waitFor(() => expect(space.updates).toHaveLength(2));
    expect(space.updates[1]).toMatchObject({ model: "m2", thinking: "medium" });
  });

  it("grants a folder and takes the grant away", async () => {
    space.bot.grants = ["/data"];
    const view = await openBots();

    expect(panelText(view, "Granted folders")).toContain("/data");

    // A granted folder goes away on its row's own button.
    flush(() => fireEvent.click(element<HTMLButtonElement>(view, ".row__remove")));
    await waitFor(() => expect(space.updates).toHaveLength(1));
    expect(space.updates[0]).toMatchObject({ bot: "b1", grant_remove: "/data" });
    await waitFor(() =>
      expect(panelText(view, "Granted folders")).toContain("No folder is granted."),
    );

    // A folder comes back through the form.
    type(view, "grant_path", "/data/project");
    await waitFor(() => expect(button(view, "Add").disabled).toBe(false));
    press(view, "Add");
    await waitFor(() => expect(space.updates).toHaveLength(2));
    expect(space.updates[1]).toMatchObject({ grant_add: "/data/project" });
    await waitFor(() => expect(panelText(view, "Granted folders")).toContain("/data/project"));
  });

  it("asks twice before it lets a Bot write everywhere", async () => {
    const view = await openBots();
    const fullAccess = field<HTMLInputElement>(view, "full_access");

    // The switch arms the risk first; it writes nothing yet. The risk says what
    // the Bot can read before the confirm button is pressed, not after.
    flush(() => fireEvent.click(fullAccess));
    expect(space.updates).toHaveLength(0);
    await waitFor(() =>
      expect(element(view, ".risk__text").textContent).toContain("read and change any file"),
    );
    const risk = () => element(view, ".risk__text").textContent ?? "";
    // With the sandbox off the Bot can also read Neha's configuration
    // directory: the saved pairing, the machine's token, and CLI sessions.
    expect(risk()).toContain("~/.config/neha");
    expect(risk()).toContain("machine's token");
    expect(risk()).toContain("CLI sessions");
    // The confirm button is on screen while the risk is, so the answer comes
    // after the risk has been read.
    expect(button(view, "Turn on full access")).toBeTruthy();

    // Taking the risk back leaves the Bot as it was.
    press(view, "Cancel");
    await waitFor(() => expect(view.container.querySelector(".risk")).toBeNull());
    expect(space.updates).toHaveLength(0);

    // The answer carries the owner's consent at the current version.
    flush(() => fireEvent.click(fullAccess));
    press(view, "Turn on full access");
    await waitFor(() => expect(space.updates).toHaveLength(1));
    expect(space.updates[0]).toMatchObject({
      bot: "b1",
      full_access: true,
      full_access_consent: 1,
    });
    await waitFor(() => expect(panelText(view, "Full access")).toContain("Full access is on."));

    // Turning it off needs no answer, and sends no consent.
    flush(() => fireEvent.click(fullAccess));
    await waitFor(() => expect(space.updates).toHaveLength(2));
    expect(space.updates[1]).toMatchObject({ full_access: false });
    expect(space.updates[1]).not.toHaveProperty("full_access_consent");
  });

  it("asks twice before it resets a Hosted Computer's Workspace", async () => {
    space.computer.kind = "hosted";
    const view = await openBots();
    await waitFor(() => expect(button(view, "Reset Workspace")).toBeTruthy());

    // A Hosted Computer takes no granted folder and no full access.
    expect(panelText(view, "Granted folders")).toContain(
      "A Hosted Computer takes no granted folder.",
    );
    expect(panelText(view, "Full access")).toContain("full access does not apply");
    expect(view.container.querySelector('[name="full_access"]')).toBeNull();

    press(view, "Reset Workspace");
    expect(space.resets).toHaveLength(0);
    await waitFor(() =>
      expect(button(view, "Press again to reset").className).toContain("btn--danger"),
    );

    press(view, "Press again to reset");
    await waitFor(() => expect(space.resets).toEqual(["p1"]));
  });

  it("distinguishes two move targets with the same name", async () => {
    space.otherComputer.name = "Twin";
    space.hostedComputer.name = "Twin";
    const view = await openBots();
    const target = field<HTMLSelectElement>(view, "move_target");
    const labels = Array.from(target.options).map((option) => option.textContent ?? "");
    expect(labels[1]).toBe(`Twin · ${space.otherComputer.id}`);
    expect(labels[2]).toBe(`Twin · ${space.hostedComputer.id}`);
  });

  it("moves a Bot to another Computer", async () => {
    const view = await openBots();
    // The page names the Computer by its name, and says who may use it.
    expect(factValue(view, "Computer", "Name")).toBe("Workstation");
    expect(factValue(view, "Computer", "Sharing")).toBe("Shared");

    // Both the other Local Computer and the Hosted Space Computer are targets:
    // a Bot may move between Hosted and Local Computers in either direction.
    const target = field<HTMLSelectElement>(view, "move_target");
    expect(target.options).toHaveLength(3);
    expect(Array.from(target.options).map((option) => option.textContent)).toEqual([
      "Choose a Computer",
      "Laptop",
      "Space Computer",
    ]);

    choose(view, "move_target", "p9");
    press(view, "Move to another Computer");
    // The first press only arms it.
    expect(space.moves).toHaveLength(0);
    await waitFor(() => expect(button(view, "Press again to move")).toBeTruthy());

    press(view, "Press again to move");
    await waitFor(() => expect(space.moves).toEqual([{ bot: "b1", computer: "p9" }]));
    // The page reads the Bot again, so it names the Computer it moved to.
    await waitFor(() => expect(factValue(view, "Computer", "Name")).toBe("Laptop"));
    expect(factValue(view, "Computer", "Workspace")).toBe("/other");
    // The new Computer is offline, as the Space knows it.
    expect(factValue(view, "Computer", "Online")).toBe("Offline");
  });

  it("moves a Bot from a Hosted Computer", async () => {
    space.computer.kind = "hosted";
    const view = await openBots();
    // A Hosted Bot moves like any other: the control is here, and the copy says
    // a move carries no files.
    await waitFor(() => expect(field<HTMLSelectElement>(view, "move_target")).toBeTruthy());
    expect(panelText(view, "Computer")).toContain("Moving does not carry files");

    choose(view, "move_target", "p9");
    press(view, "Move to another Computer");
    await waitFor(() => expect(button(view, "Press again to move")).toBeTruthy());
    press(view, "Press again to move");
    await waitFor(() => expect(space.moves).toEqual([{ bot: "b1", computer: "p9" }]));
  });

  it("adds a rule and takes it away", async () => {
    space.rules = [{ ...askRule(), command_prefix: null, effect: "allow" }];
    const view = await openBots();

    expect(panelText(view, "Ask and Allow rules")).toContain("Allow");
    expect(panelText(view, "Ask and Allow rules")).toContain("shell");
    // A rule that names no prefix covers every action of its tool.
    expect(element(view, ".rows .row__meta").textContent).toBe("Every shell action");

    flush(() => fireEvent.click(element<HTMLButtonElement>(view, ".row__remove")));
    await waitFor(() => expect(space.ruleRemoves).toEqual(["rule1"]));
    await waitFor(() =>
      expect(panelText(view, "Ask and Allow rules")).toContain("No rule is set."),
    );

    // A new rule names its effect, its tool, and what it covers. The tool comes
    // from the set every Bot has, so the page cannot name one the Space would
    // refuse.
    choose(view, "rule_effect", "ask");
    choose(view, "rule_tool", "write_file");
    type(view, "rule_path", "/data");
    await waitFor(() => expect(button(view, "Add rule").disabled).toBe(false));
    press(view, "Add rule");
    await waitFor(() => expect(space.ruleAdds).toHaveLength(1));
    expect(space.ruleAdds[0]).toMatchObject({
      bot: "b1",
      effect: "ask",
      tool: "write_file",
      path_prefix: "/data",
      command_prefix: null,
    });
    await waitFor(() => expect(element(view, ".rows .row__name").textContent).toBe("write_file"));
    expect(panelText(view, "Ask and Allow rules")).toContain("Ask");
    expect(element(view, ".rows .row__meta").textContent).toContain("/data");
  });

  it("lists a Bot's memory and clears an entry", async () => {
    const rule = memoryOf({ memory: "mem1", text: "Always answer briefly." });
    const note = memoryOf({
      memory: "mem2",
      kind: "note",
      author: "b1",
      author_bot: true,
      source: "r7",
      source_kind: "run",
      text: "The project runs on Node 22.",
      size: 4096,
    });
    space.memories = { b1: [rule, note] };
    const view = await openBots();

    const memory = () => memoryPanel(view).textContent ?? "";
    // The two kinds sit in one list, rules first, each with the room it has
    // left: what its entries use, against the kind's limit.
    expect(memory().indexOf("Rules")).toBeLessThan(memory().indexOf("Notes"));
    expect(memory()).toContain("Always answer briefly.");
    expect(memory()).toContain("The project runs on Node 22.");
    expect(memory()).toContain(
      `${formatBytes(rule.size)} / ${formatBytes(space.memoryLimits.rule)}`,
    );
    expect(memory()).toContain(
      `${formatBytes(note.size)} / ${formatBytes(space.memoryLimits.note)}`,
    );

    // A rule names the owner whose words gave it; a note names the Bot. Both
    // name the Message or run that wrote them.
    expect(memoryRow(view, "Always answer briefly.").textContent).toContain("Owner");
    expect(memoryRow(view, "Always answer briefly.").textContent).toContain("From a message m1");
    expect(memoryRow(view, "The project runs on Node 22.").textContent).toContain("Bot");
    expect(memoryRow(view, "The project runs on Node 22.").textContent).toContain("From a run r7");

    // The owner clears one entry; the list and the room its kind uses follow.
    flush(() =>
      fireEvent.click(memoryRow(view, "Always answer briefly.").querySelector(".row__remove")!),
    );
    // The removal names the Bot the entry belongs to, so the Space can refuse
    // an entry that belongs to another Bot.
    await waitFor(() => expect(space.memoryRemoves).toEqual([{ bot: "b1", memory: "mem1" }]));
    await waitFor(() => expect(memory()).not.toContain("Always answer briefly."));
    await waitFor(() =>
      expect(memory()).toContain(`${formatBytes(0)} / ${formatBytes(space.memoryLimits.rule)}`),
    );

    // The last entry goes too: the page keeps the room each kind has left, and
    // the advice that returns once nothing is remembered.
    flush(() =>
      fireEvent.click(
        memoryRow(view, "The project runs on Node 22.").querySelector(".row__remove")!,
      ),
    );
    await waitFor(() =>
      expect(space.memoryRemoves).toEqual([
        { bot: "b1", memory: "mem1" },
        { bot: "b1", memory: "mem2" },
      ]),
    );
    await waitFor(() => expect(memory()).toContain("Ask the Bot in a Chat to remember something."));
    expect(memory()).toContain(`${formatBytes(0)} / ${formatBytes(space.memoryLimits.rule)}`);
    expect(memory()).toContain(`${formatBytes(0)} / ${formatBytes(space.memoryLimits.note)}`);
  });

  it("groups memory by kind and then by the scope it applies in", async () => {
    space.memories = {
      b1: [
        // The Space lists a Chat's entry before the owner's here, so the page
        // is what puts the scopes in their fixed order.
        memoryOf({
          memory: "mem5",
          scope: "user",
          scope_id: "h9",
          text: "Keep replies short.",
        }),
        memoryOf({ memory: "mem6", text: "Always answer briefly." }),
        memoryOf({
          memory: "mem7",
          kind: "note",
          author: "b1",
          author_bot: true,
          text: "Project uses Node 22.",
        }),
      ],
    };
    const view = await openBots();

    const memory = () => memoryPanel(view).textContent ?? "";
    // Inside a kind, the owner's entries come before the ones that apply in one
    // Chat.
    expect(memory().indexOf("Rules · Owner")).toBeGreaterThanOrEqual(0);
    expect(memory().indexOf("Rules · User")).toBeGreaterThanOrEqual(0);
    expect(memory().indexOf("Rules · Owner")).toBeLessThan(memory().indexOf("Rules · User"));
    // The Notes kind follows the Rules kind, and names its own scope.
    expect(memory().indexOf("Rules · User")).toBeLessThan(memory().indexOf("Notes · Owner"));
    // A scope the kind holds no entry in shows no group.
    expect(memory()).not.toContain("Rules · Group");
    expect(memory()).not.toContain("Notes · User");
    // Each row still names its own scope too.
    expect(memoryRow(view, "Always answer briefly.").textContent).toContain("Owner");
    expect(memoryRow(view, "Keep replies short.").textContent).toContain("User");
  });

  it("shows a memory entry it does not read with its metadata only", async () => {
    space.memories = {
      b1: [
        memoryOf({
          memory: "mem3",
          scope: "user",
          scope_id: "h9",
          author: "u2",
          readable: false,
          size: 777,
        }),
      ],
    };
    const view = await openBots();

    // The owner audits the entry without reading it: its scope, author, source,
    // and size, and no text.
    const row = memoryRow(view, "777 B");
    expect(row.querySelector(".row__name")).toBeNull();
    expect(row.textContent).toContain("User");
    expect(row.textContent).toContain("From a message m1");
    // The owner still clears it.
    flush(() => fireEvent.click(row.querySelector(".row__remove")!));
    await waitFor(() => expect(space.memoryRemoves).toEqual([{ bot: "b1", memory: "mem3" }]));
  });

  it("says how to add the first memory entry and what each kind may hold", async () => {
    const view = await openBots();

    // With no entry, the page points the User at a Chat. It offers no control
    // that adds or changes one: a User does that in a Chat.
    const memory = memoryPanel(view).textContent ?? "";
    expect(memory).toContain("Ask the Bot in a Chat to remember something.");
    expect(memory).toContain("Rules");
    expect(memory).toContain("Notes");
    // An empty memory still names the room each kind has left.
    expect(memory).toContain(`${formatBytes(0)} / ${formatBytes(space.memoryLimits.rule)}`);
    expect(memory).toContain(`${formatBytes(0)} / ${formatBytes(space.memoryLimits.note)}`);
    expect(memoryPanel(view).querySelector("form")).toBeNull();
    expect(memoryPanel(view).querySelector("input")).toBeNull();
  });

  it("shows a memory entry another client wrote", async () => {
    const view = await openBots();
    const memory = () => memoryPanel(view).textContent ?? "";
    expect(memory()).toContain("Ask the Bot in a Chat to remember something.");

    space.memories = {
      b1: [
        memoryOf({
          memory: "mem4",
          kind: "note",
          author: "b1",
          author_bot: true,
          source: "r8",
          source_kind: "run",
          text: "Deploys run from main.",
        }),
      ],
    };
    record("neha.memory.added", { memory: "mem4", bot: "b1" });
    await waitFor(() => expect(memory()).toContain("Deploys run from main."));

    space.memories = { b1: [] };
    record("neha.memory.removed", { memory: "mem4" });
    await waitFor(() => expect(memory()).toContain("Ask the Bot in a Chat to remember something."));
  });

  it("says why the Space refused a change", async () => {
    space.refuse.bot_update = new ProtocolError("forbidden", "The Space does not allow that.");
    const view = await openBots();

    type(view, "bot_name", "Nova");
    press(view, "Save name");

    // The page names the refusal where the change was asked for, and keeps the
    // Bot's own name.
    await waitFor(() => expect(space.asked).toContain("bot_update"));
    await waitFor(() =>
      expect(element(view, '[role="alert"]').textContent).toBe(
        "This Space does not allow that change.",
      ),
    );
    await waitFor(() => expect(element(view, ".page__titles h1").textContent).toBe("Scout"));
  });

  it("connects a Provider from the catalog", async () => {
    space.providers = [];
    space.vendors = [
      {
        id: "openai",
        name: "OpenAI",
        protocol: "openai_responses",
        selectable: true,
        api: "https://api.openai.com/v1",
        npm: "@ai-sdk/openai",
        env: ["OPENAI_API_KEY"],
        doc: null,
        model_count: 12,
      },
    ];
    const view = await openSettings();

    expect(element(view, ".side-empty__title").textContent).toBe("No Provider yet");
    const vendors = field<HTMLSelectElement>(view, "vendor");
    expect(vendors.options).toHaveLength(2);
    expect(vendors.options[1]?.textContent).toBe("OpenAI");

    // The key is required, so the form stays closed until it is given.
    await waitFor(() => expect(button(view, "Connect Provider").disabled).toBe(true));
    choose(view, "vendor", "openai");
    type(view, "api_key", "sk-test");
    await waitFor(() => expect(button(view, "Connect Provider").disabled).toBe(false));

    // The advanced fields stay out of the way until they are asked for.
    expect(view.container.querySelector('[name="connect_base_url"]')).toBeNull();
    press(view, "Advanced");
    await waitFor(() => expect(field<HTMLInputElement>(view, "connect_base_url")).toBeTruthy());

    press(view, "Connect Provider");
    await waitFor(() => expect(space.connects).toHaveLength(1));
    expect(space.connects[0]).toMatchObject({ vendor: "openai", api_key: "sk-test" });
    // The new Provider is listed, and the key is never shown again.
    await waitFor(() =>
      expect(element(view, ".row__name").textContent).toBe("https://api.openai.com/v1"),
    );
    await waitFor(() => expect(field<HTMLInputElement>(view, "api_key").value).toBe(""));
    await waitFor(() => expect(element(view, ".page__note").textContent).toBe("Models refreshed"));
  });

  it("makes one Model the default every Bot starts from", async () => {
    space.models = [
      thinker,
      { ...quiet, tools: true, is_default: false, thinking_levels: ["low"] },
    ];
    const view = await openSettings("Models");

    // The default Model says so, and the others offer the change.
    const rows = view.container.querySelectorAll(".row");
    expect(rows[0]?.textContent).toContain("Space default");
    expect(rows[1]?.textContent).toContain("Make default");
    expect(rows[0]?.textContent).toContain("Context 400000");

    press(view, "Make default");
    await waitFor(() => expect(space.defaults).toEqual(["m2"]));
    await waitFor(() =>
      expect(element(view, ".page__note").textContent).toBe("Default Model saved"),
    );
    // The default moved to the Model that was chosen.
    await waitFor(() => {
      const after = view.container.querySelectorAll(".row");
      expect(after[1]?.textContent).toContain("Space default");
      expect(after[0]?.textContent).toContain("Make default");
    });
  });

  it("shows what the catalog's cache holds and refreshes it", async () => {
    space.catalog = {
      source: "https://models.dev/api.json",
      builtin: false,
      etag: null,
      fetched_at: 1_764_000_000,
      checked_at: 1_764_000_000,
      age_seconds: 60,
      stale: true,
      provider_count: 42,
      cache_path: "/home/catalog.json",
      model_count: 900,
    };
    const view = await openSettings("Catalog");

    const facts = view.container.querySelectorAll(".fact");
    expect(facts[0]?.textContent).toContain("https://models.dev/api.json");
    expect(facts[1]?.textContent).toContain("Nov");
    expect(facts[3]?.textContent).toContain("42");
    expect(facts[4]?.textContent).toContain("900");
    // A cache that is due says so.
    expect(facts[5]?.textContent).toContain("A newer catalog is due.");

    press(view, "Refresh catalog");
    await waitFor(() => expect(space.catalogRefreshes).toBe(1));
    await waitFor(() => expect(element(view, ".page__note").textContent).toBe("Catalog updated"));
    await waitFor(() => expect(facts[5]?.textContent).toContain("Up to date"));
  });

  it("refreshes the Models a Provider offers", async () => {
    space.models = [thinker];
    const view = await openSettings();
    expect(element(view, ".row__name").textContent).toBe("http://provider");

    press(view, "Refresh Models");
    await waitFor(() => expect(space.refreshes).toEqual(["v1"]));
    await waitFor(() => expect(element(view, ".page__note").textContent).toBe("Models refreshed"));
  });

  it("keeps the last good catalog when the Space has none of its own", async () => {
    space.catalog = {
      source: "builtin",
      builtin: true,
      etag: null,
      fetched_at: null,
      checked_at: null,
      age_seconds: null,
      stale: false,
      provider_count: 3,
      cache_path: "/home/catalog.json",
      model_count: 40,
    };
    const view = await openSettings("Catalog");

    const facts = view.container.querySelectorAll(".fact");
    expect(facts[0]?.textContent).toContain("Built-in snapshot");
    // A cache the Space never wrote names no day.
    expect(facts[1]?.textContent).toContain("never");
    expect(panelText(view, "Catalog")).toContain("The last known good catalog is kept.");
  });

  it("says when nothing is connected yet", async () => {
    space.providers = [];
    space.models = [];
    space.chats = [];
    const view = renderApp();
    await waitFor(() => expect(element(view, ".rail__item:nth-of-type(3)")).toBeTruthy());

    // The Providers are empty until one is connected.
    flush(() => fireEvent.click(element(view, ".rail__item:nth-of-type(4)")));
    await waitFor(() =>
      expect(element(view, ".side-empty__title").textContent).toBe("No Provider yet"),
    );

    // So are the Models, which come with them.
    flush(() => fireEvent.click(element(view, ".conv:nth-of-type(2)")));
    await waitFor(() =>
      expect(element(view, ".side-empty__title").textContent).toBe("No Model yet"),
    );

    // A Space with no Bot has no Bot page to open.
    flush(() => fireEvent.click(element(view, ".rail__item:nth-of-type(3)")));
    await waitFor(() =>
      expect(element(view, ".side-empty__title").textContent).toBe("No Bots yet"),
    );
    expect(view.container.querySelector(".page__body")).toBeNull();
  });

  it("shows a rename another client made", async () => {
    const view = await openChat();
    // Another client renamed the Bot: the record says so.
    space.bot.name = "Renamed";
    record("neha.bot.renamed", { bot: "b1", name: "Renamed" });
    // The Chat on screen names the Bot, so it shows the new name at once.
    await waitFor(() => expect(element(view, ".chat__titles h1").textContent).toBe("Renamed"));

    // The management surface reads the Bot again, so its page names the new Bot
    // too.
    flush(() => fireEvent.click(element(view, ".rail__item:nth-of-type(3)")));
    await waitFor(() => expect(view.container.querySelectorAll(".conv")).toHaveLength(1));
    flush(() => fireEvent.click(element(view, ".conv")));
    await waitFor(() => expect(element(view, ".page__titles h1").textContent).toBe("Renamed"));
    await drain();
  });

  it("takes a Model back to the Space's default", async () => {
    space.models = [thinker, plain];
    const view = await openBots();

    // The Bot's own choice travels as the choice it made.
    choose(view, "bot_model", "m1");
    choose(view, "bot_thinking", "high");
    press(view, "Save Model");
    await waitFor(() => expect(space.updates).toHaveLength(1));
    expect(space.updates[0]).toMatchObject({ model: "m1", thinking: "high" });
    await waitFor(() => expect(button(view, "Save Model").disabled).toBe(true));

    // A Model that cannot think sends no level at all: an empty one would leave
    // the Bot on a level the Model does not have.
    choose(view, "bot_model", "m3");
    press(view, "Save Model");
    await waitFor(() => expect(space.updates).toHaveLength(2));
    expect(space.updates[1]).toMatchObject({ model: "m3", reset_thinking: true });
    expect(space.updates[1]).not.toHaveProperty("thinking");
    await waitFor(() => expect(button(view, "Save Model").disabled).toBe(true));

    // Back to the default is a field of its own: the Space holds the default,
    // and an absent Model leaves the Bot's own choice alone. The Bot names no
    // level by then, so going back to the default Model names none either: it
    // follows the Space's level instead of the default Model's cheapest.
    choose(view, "bot_model", "");
    press(view, "Save Model");
    await waitFor(() => expect(space.updates).toHaveLength(3));
    expect(space.updates[2]).toMatchObject({ bot: "b1", reset_model: true });
    expect(space.updates[2]).not.toHaveProperty("model");
    expect(space.updates[2]).not.toHaveProperty("thinking");
    expect(space.updates[2]).not.toHaveProperty("reset_thinking");
    // The page shows the Bot following the Space's default again.
    await waitFor(() => expect(panelText(view, "Model")).toContain("Space default"));
  });

  it("takes a level back to the Space's default", async () => {
    space.models = [thinker, plain];
    space.bot.model = "m1";
    space.bot.thinking = "high";
    const view = await openBots();

    // The page shows the level the Bot names, not the Space's default.
    expect(field<HTMLSelectElement>(view, "bot_thinking").value).toBe("high");

    // Clearing only the level is a field of its own, and it leaves the Model
    // the Bot chose alone.
    choose(view, "bot_thinking", "");
    press(view, "Save Model");
    await waitFor(() => expect(space.updates).toHaveLength(1));
    expect(space.updates[0]).toMatchObject({ bot: "b1", reset_thinking: true });
    expect(space.updates[0]).not.toHaveProperty("thinking");
    expect(space.updates[0]).not.toHaveProperty("model");
    expect(space.updates[0]).not.toHaveProperty("reset_model");

    // The Bot follows the Space's level again, so the page stops offering to
    // save what the Bot already holds.
    await waitFor(() => expect(button(view, "Save Model").disabled).toBe(true));
    expect(field<HTMLSelectElement>(view, "bot_thinking").value).toBe("");
    await drain();
  });

  it("keeps a level the Bot never chose when it goes back to the default Model", async () => {
    space.models = [thinker, plain];
    // The Bot names a Model but no level: it follows the Space's.
    space.bot.model = "m1";
    space.bot.thinking = null;
    const view = await openBots();

    expect(field<HTMLSelectElement>(view, "bot_thinking").value).toBe("");
    expect(panelText(view, "Model")).toContain("Space default");

    // Going back to the default Model names no level: the Bot keeps following
    // the Space's, which is what it was doing before it named a Model.
    choose(view, "bot_model", "");
    press(view, "Save Model");
    await waitFor(() => expect(space.updates).toHaveLength(1));
    expect(space.updates[0]).toMatchObject({ bot: "b1", reset_model: true });
    expect(space.updates[0]).not.toHaveProperty("model");
    expect(space.updates[0]).not.toHaveProperty("thinking");
    expect(space.updates[0]).not.toHaveProperty("reset_thinking");
    await drain();
  });

  it("drops a Bot page write that lands after another Bot was opened", async () => {
    // A second Chat gives the Space a second Bot, which is where the User goes
    // while the first page's write is still on its way.
    space.chats = [space.chat, space.otherChat];
    const view = renderApp();
    await waitFor(() => expect(element(view, ".rail__item:nth-of-type(3)")).toBeTruthy());
    flush(() => fireEvent.click(element(view, ".rail__item:nth-of-type(3)")));
    await waitFor(() => expect(view.container.querySelectorAll(".conv")).toHaveLength(2));
    flush(() => fireEvent.click(element(view, ".conv")));
    await waitFor(() => expect(element(view, ".page__body")).toBeTruthy());
    type(view, "bot_name", "Nova");
    press(view, "Save name");
    await waitFor(() => expect(space.asked).toContain("bot_update"));

    // The answer lands after the User opened another Bot's page.
    await openOtherBot(view);
    await drain();

    // The write the first page asked for never reaches the page now open: its
    // name is the one the second Bot's page holds.
    expect(element(view, ".page__titles h1").textContent).toBe(space.otherBot.name);
    expect(view.container.querySelector('[name="bot_name"]')).toHaveProperty(
      "value",
      space.otherBot.name,
    );
    // The Bot the User closed keeps the name its own page asked for.
    expect(space.bot.name).toBe("Nova");
  });

  it("clears the Workspace of the Computer the Bot on screen works on", async () => {
    space.computer.kind = "hosted";
    space.bot.computer = "p5";
    const view = await openBots();
    await waitFor(() => expect(panelText(view, "Computer")).toContain("/hosted"));

    press(view, "Reset Workspace");
    await waitFor(() => expect(button(view, "Press again to reset")).toBeTruthy());
    press(view, "Press again to reset");
    await waitFor(() => expect(space.resets).toEqual(["p5"]));
  });

  it("lists every Computer the Space can use in Settings", async () => {
    space.bot.computer = "p1";
    space.otherBot.computer = "p9";
    const view = await openSettings("Computers");

    const rows = view.container.querySelectorAll(".rows .row");
    // Both paired Local Computers and the Hosted one are listed, including the
    // one no Bot works on. The name leads each row; the kind, sharing, and state
    // follow it, and the Workspace stays machine data further down.
    expect(rows).toHaveLength(3);
    expect(rows[0]?.querySelector(".row__name")?.textContent).toBe("Workstation");
    expect(rows[0]?.textContent).toContain("Local Computer");
    expect(rows[0]?.textContent).toContain("Shared");
    expect(rows[0]?.textContent).toContain("Online");
    expect(rows[0]?.textContent).toContain("/w");
    // A Computer's Bots and what each may write are on its row.
    expect(rows[0]?.textContent).toContain("Scout");
    expect(rows[0]?.textContent).toContain("Only its Workspace");
    expect(rows[1]?.textContent).toContain("No Bot works here");
    // The Space Computer says that it is the one the User received.
    expect(rows[2]?.querySelector(".row__name")?.textContent).toBe("Space Computer");
    expect(rows[2]?.textContent).toContain("Hosted Computer");
    expect(rows[2]?.querySelector(".chip--default")?.textContent).toBe("Space Computer");

    // The Bot chip opens that Bot's page.
    flush(() => fireEvent.click(button(view, "Scout · Only its Workspace")));
    await waitFor(() => expect(element(view, ".page__titles h1").textContent).toBe("Scout"));
    // Coming back returns to the Computers list, not to the Chats.
    flush(() => fireEvent.click(element(view, ".page__back")));
    await waitFor(() => expect(panelText(view, "Computers")).toContain("Workstation"));
  });

  it("creates a shared Hosted Computer from the Computers list", async () => {
    const view = await openSettings("Computers");

    // The form asks for a name and who may use it, and waits for the name.
    await waitFor(() => expect(button(view, "Create Computer").disabled).toBe(true));

    type(view, "computer_name", "Build Box");
    choose(view, "computer_sharing", "shared");
    await waitFor(() => expect(button(view, "Create Computer").disabled).toBe(false));

    press(view, "Create Computer");
    await waitFor(() =>
      expect(space.computerCreates).toEqual([{ name: "Build Box", sharing: "shared" }]),
    );
    // The new Computer is listed with the name and sharing it was given.
    await waitFor(() => {
      const rows = view.container.querySelectorAll(".rows .row");
      const created = rows[rows.length - 1];
      expect(created?.querySelector(".row__name")?.textContent).toBe("Build Box");
      expect(created?.textContent).toContain("Hosted Computer");
      expect(created?.textContent).toContain("Shared");
    });
    await waitFor(() => expect(element(view, ".page__note").textContent).toBe("Computer created"));
    // The form starts over, so a second Computer needs a name of its own.
    expect(field<HTMLInputElement>(view, "computer_name").value).toBe("");
  });

  it("makes an exclusive Hosted Computer shared", async () => {
    space.hostedComputer.sharing = "exclusive";
    const view = await openSettings("Computers");

    const row = () => view.container.querySelectorAll(".rows .row")[2];
    expect(row()?.textContent).toContain("Exclusive");
    // Only an exclusive Hosted Computer offers the change.
    await waitFor(() => expect(button(view, "Make shared")).toBeTruthy());

    press(view, "Make shared");
    await waitFor(() =>
      expect(space.computerUpdates).toEqual([{ computer: "p5", sharing: "shared" }]),
    );
    await waitFor(() =>
      expect(element(view, ".page__note").textContent).toBe("Computer is shared"),
    );
    // The row and the action follow the sharing the Space now holds.
    await waitFor(() => {
      expect(row()?.textContent).toContain("Shared");
      expect(row()?.textContent).not.toContain("Exclusive");
    });
    expect(
      Array.from(view.container.querySelectorAll("button")).some(
        (button) => button.textContent?.trim() === "Make shared",
      ),
    ).toBe(false);
  });

  it("names every Computer it lists, even one with no workspace", async () => {
    // A Computer the User paired does not always name a workspace. The row is
    // named by the name the Space gave it rather than by that path, so no row
    // is a blank, and its kind and sharing still say what it is.
    space.computer.workspace = "";
    space.otherComputer.workspace = "";
    const view = await openSettings("Computers");

    const rows = Array.from(view.container.querySelectorAll(".rows .row"));
    expect(rows.map((row) => row.querySelector(".row__name")?.textContent)).toEqual([
      "Workstation",
      "Laptop",
      "Space Computer",
    ]);
    // With no Workspace to say it either, the row still says what the Computer
    // is and whether it is here.
    expect(rows[0]?.textContent).toContain("Local Computer");
    expect(rows[0]?.textContent).toContain("Shared");
    expect(rows[0]?.textContent).toContain("Online");
    expect(rows[1]?.textContent).toContain("Offline");
    expect(rows[2]?.textContent).toContain("Hosted Computer");
  });

  it("resets a Hosted Computer from the Computers list", async () => {
    space.bot.computer = "p1";
    const view = await openSettings("Computers");

    const rows = view.container.querySelectorAll(".rows .row");
    // Only a Hosted Computer offers a reset: a Local one belongs to its machine.
    expect(rows[0]?.textContent).not.toContain("Reset Workspace");
    expect(rows[2]?.textContent).toContain("Reset Workspace");
    flush(() => fireEvent.click(button(view, "Reset Workspace")));
    // The first press only arms it, and says what it clears.
    expect(space.resets).toHaveLength(0);
    await waitFor(() => expect(button(view, "Press again to reset")).toBeTruthy());

    press(view, "Press again to reset");
    await waitFor(() => expect(space.resets).toEqual(["p5"]));
    await waitFor(() => expect(element(view, ".page__note").textContent).toBe("Workspace cleared"));
  });

  it("says when one Provider offers no Models", async () => {
    space.providers = [
      { id: "v1", protocol: "chat_completions", base_url: "http://one", vendor: null },
      { id: "v2", protocol: "chat_completions", base_url: "http://two", vendor: null },
    ];
    space.models = [thinker];
    const view = await openSettings("Models");

    const rows = view.container.querySelectorAll(".rows .row");
    // The Provider with Models lists them, and the one without says so where
    // its Models would be.
    expect(rows).toHaveLength(2);
    expect(rows[0]?.textContent).toContain("Atlas");
    expect(rows[0]?.textContent).toContain("http://one");
    expect(rows[1]?.textContent).toContain("This Provider offers no Models yet.");
    // A Space with no Provider at all keeps its own empty state, which the
    // "says when nothing is connected yet" test reads.
  });

  it("keeps a Model that cannot call tools from being the default", async () => {
    space.models = [thinker, quiet];
    const view = await openSettings("Models");

    const rows = view.container.querySelectorAll(".row");
    expect(rows[0]?.textContent).toContain("Space default");
    // The quiet Model cannot answer a Chat, so the Space would refuse it.
    const offer = rows[1]?.querySelector("button") as HTMLButtonElement;
    expect(offer.textContent).toBe("Make default");
    expect(offer.disabled).toBe(true);
    expect(offer.title).toBe("This Model cannot call tools, so it cannot be the default.");
    // Pressing it writes nothing.
    flush(() => fireEvent.click(offer));
    await drain();
    expect(space.defaults).toEqual([]);
  });

  it("sends no override when Advanced stays closed", async () => {
    space.providers = [];
    space.vendors = [
      {
        id: "openai",
        name: "OpenAI",
        protocol: "openai_responses",
        selectable: true,
        api: "https://api.openai.com/v1",
        npm: "@ai-sdk/openai",
        env: ["OPENAI_API_KEY"],
        doc: null,
        model_count: 12,
      },
    ];
    const view = await openSettings();
    choose(view, "vendor", "openai");
    type(view, "api_key", "sk-test");

    // The catalog's own protocol and base URL stand, so nothing is overridden.
    press(view, "Connect Provider");
    await waitFor(() => expect(space.connects).toHaveLength(1));
    expect(space.connects[0]).toEqual({ vendor: "openai", api_key: "sk-test" });
    expect(space.connects[0]).not.toHaveProperty("base_url");
    expect(space.connects[0]).not.toHaveProperty("protocol");
    expect(space.connects[0]).not.toHaveProperty("thinking");
    // The new Provider carries the catalog's preset, not one the page made up.
    await waitFor(() =>
      expect(element(view, ".row__name").textContent).toBe("https://api.openai.com/v1"),
    );
  });

  it("never sends one vendor's base URL with another vendor's key", async () => {
    space.providers = [];
    space.vendors = [
      {
        id: "openai",
        name: "OpenAI",
        protocol: "openai_responses",
        selectable: true,
        api: "https://api.openai.com/v1",
        npm: "@ai-sdk/openai",
        env: ["OPENAI_API_KEY"],
        doc: null,
        model_count: 12,
      },
      {
        id: "anthropic",
        name: "Anthropic",
        protocol: "anthropic_messages",
        selectable: true,
        api: "https://api.anthropic.com/v1",
        npm: "@ai-sdk/openai",
        env: ["OPENAI_API_KEY"],
        doc: null,
        model_count: 4,
      },
    ];
    const view = await openSettings();

    // The Admin connected one vendor through an override they filled in.
    choose(view, "vendor", "openai");
    type(view, "api_key", "sk-openai");
    press(view, "Advanced");
    type(view, "connect_base_url", "https://proxy.example/v1");
    press(view, "Connect Provider");
    await waitFor(() => expect(space.connects).toHaveLength(1));
    expect(space.connects[0]).toMatchObject({
      vendor: "openai",
      base_url: "https://proxy.example/v1",
    });
    // The key and the override are never shown again once they are spent.
    await waitFor(() => expect(field<HTMLInputElement>(view, "api_key").value).toBe(""));
    await waitFor(() => expect(field<HTMLInputElement>(view, "connect_base_url").value).toBe(""));

    // The next vendor is chosen and only its own key is pasted. The base URL
    // and the overrides of the first vendor are gone with it.
    choose(view, "vendor", "anthropic");
    expect(field<HTMLInputElement>(view, "api_key").value).toBe("");
    expect(field<HTMLInputElement>(view, "connect_base_url").value).toBe("");
    type(view, "api_key", "sk-anthropic");
    press(view, "Connect Provider");
    await waitFor(() => expect(space.connects).toHaveLength(2));
    expect(space.connects[1]).toEqual({ vendor: "anthropic", api_key: "sk-anthropic" });
    // The second Provider carries the catalog's preset for its own vendor.
    await waitFor(() => expect(space.providers[1]?.base_url).toBe("https://api.anthropic.com/v1"));
  });

  it("adds no rule to a Bot a Hosted Computer runs", async () => {
    space.computer.kind = "hosted";
    space.rules = [askRule()];
    const view = await openBots();

    // The rules the Bot still holds are listed, and can be taken away.
    expect(panelText(view, "Ask and Allow rules")).toContain("shell");

    // The page says why nothing can be added, and offers no form to add it.
    const rules = panelText(view, "Ask and Allow rules");
    expect(rules).toContain("A Hosted Computer applies no Ask or Allow rule");
    expect(view.container.querySelector('[name="rule_effect"]')).toBeNull();
    expect(view.container.querySelector('[name="rule_tool"]')).toBeNull();
    expect(
      Array.from(view.container.querySelectorAll("button")).some(
        (known) => known.textContent?.trim() === "Add rule",
      ),
    ).toBe(false);
  });

  it("offers no grant or full access a Computer's daemon refuses", async () => {
    // The daemon on this Computer names no capability it does not have: what it
    // allows is decided by the machine.
    space.computer.supports_grants = false;
    space.computer.supports_full_access = false;
    const view = await openBots();

    expect(panelText(view, "Granted folders")).toContain(
      "What this Computer allows is decided by its machine.",
    );
    expect(view.container.querySelector('[name="grant_path"]')).toBeNull();
    expect(panelText(view, "Full access")).toContain(
      "What this Computer allows is decided by its machine.",
    );
    expect(view.container.querySelector('[name="full_access"]')).toBeNull();
    // Pressing nothing writes nothing.
    expect(space.updates).toHaveLength(0);
  });

  it("shows the default Model another client chose", async () => {
    space.models = [
      thinker,
      { ...quiet, tools: true, is_default: false, thinking_levels: ["low"] },
    ];
    const view = await openSettings("Models");

    const before = view.container.querySelectorAll(".row");
    expect(before[0]?.textContent).toContain("Space default");
    expect(before[1]?.textContent).toContain("Make default");

    // Another client made the second Model the one every Bot starts from.
    space.models = space.models.map((known) => ({ ...known, is_default: known.id === "m2" }));
    record("neha.provider.models_updated", { provider: "v1" });
    await waitFor(() => {
      const after = view.container.querySelectorAll(".row");
      expect(after[0]?.textContent).toContain("Make default");
      expect(after[1]?.textContent).toContain("Space default");
    });
  });

  it("marks a reply that arrived while a management page was open", async () => {
    const view = await openChat();
    // The Chat is being read: no mark yet.
    expect(view.container.querySelector(".conv__badge")).toBeNull();

    // The User opens the Settings surface, which leaves the Chat open behind it.
    flush(() => fireEvent.click(element(view, ".rail__item:nth-of-type(4)")));
    await waitFor(() => expect(element(view, ".page__body")).toBeTruthy());

    // A reply to the Chat the User is not reading is new activity: the Chats
    // list is behind the Settings surface, and it says so when it comes back.
    record("neha.message.added", { chat: "h1", text: "on its way", run: "r1" });
    flush(() => fireEvent.click(element(view, ".page__back")));
    await waitFor(() => expect(view.container.querySelector(".page__body")).toBeNull());
    expect(element(view, ".conv__badge").textContent).toBe("New activity");
    // Opening the Chat answers it.
    flush(() => fireEvent.click(element(view, ".conv")));
    await waitFor(() => expect(view.container.querySelector(".conv__badge")).toBeNull());
  });

  it("reaches a management surface through the rail on a narrow viewport", async () => {
    // A narrow viewport shows one surface at a time, so the rail is the only way
    // to a management surface: it stays, and Back returns to where the User was.
    const wide = window.matchMedia;
    try {
      Object.defineProperty(window, "matchMedia", {
        writable: true,
        value: (query: string) => ({
          matches: true,
          media: query,
          addEventListener: () => undefined,
          removeEventListener: () => undefined,
        }),
      });
      const view = renderApp();
      await waitFor(() => expect(element(view, ".rail__item:nth-of-type(4)")).toBeTruthy());

      // The rail opens the Settings surface, and Back returns to the Chats the
      // User came from, with no Chat open.
      flush(() => fireEvent.click(element(view, ".rail__item:nth-of-type(4)")));
      await waitFor(() => expect(element(view, ".page__body")).toBeTruthy());
      expect(panelText(view, "Providers")).toBeTruthy();
      flush(() => fireEvent.click(element(view, ".page__back")));
      await waitFor(() => expect(view.container.querySelector(".page__body")).toBeNull());
      expect(element(view, ".select-chat")).toBeTruthy();

      // A Bot page opened from the Settings returns to the Settings, which is
      // where the User was before they opened it.
      flush(() => fireEvent.click(element(view, ".rail__item:nth-of-type(4)")));
      await waitFor(() => expect(element(view, ".page__body")).toBeTruthy());
      flush(() => fireEvent.click(button(view, "Computers")));
      await waitFor(() => expect(panelText(view, "Computers")).toBeTruthy());
      flush(() => fireEvent.click(button(view, "Scout · Only its Workspace")));
      await waitFor(() => expect(element(view, ".page__titles h1").textContent).toBe("Scout"));
      flush(() => fireEvent.click(element(view, ".page__back")));
      await waitFor(() => expect(panelText(view, "Computers")).toBeTruthy());
    } finally {
      window.matchMedia = wide;
    }
  });

  it("asks for a Computer's word before it says what the shell runs as", async () => {
    // A Space that restarted has not heard from this Computer yet: nothing is
    // known about its machine, so the summary asks rather than claiming either
    // a fence or the machine user's own rights.
    space.computer.sandboxed = null;
    const view = await openSettings("Computers");

    expect(panelText(view, "Computers")).toContain(
      "Waiting for this Computer to say what it allows",
    );
    expect(panelText(view, "Computers")).not.toContain("Shell runs as the machine user");
    expect(panelText(view, "Computers")).not.toContain("Only its Workspace");
    await drain();
  });

  it("says what a Windows Computer's shell really runs as", async () => {
    // An online Windows Local daemon serves no fence: it runs the Bot's shell
    // with the machine user's own rights, so a summary that named the Bot's
    // folders would describe a fence that is not there.
    space.computer.sandboxed = false;
    space.computer.supports_grants = false;
    space.computer.supports_full_access = false;
    const view = await openSettings("Computers");

    expect(panelText(view, "Computers")).toContain("Shell runs as the machine user");
    // The Bot's own page says the same thing about the same Computer.
    flush(() => fireEvent.click(element(view, ".chip--button")));
    await waitFor(() => expect(element(view, ".page__titles h1").textContent).toBe("Scout"));
    expect(panelText(view, "Granted folders")).toContain(
      "What this Computer allows is decided by its machine.",
    );
    await drain();
  });

  it("opens the Settings sections before their detail on a narrow viewport", async () => {
    // A narrow viewport shows one surface at a time, so the Settings surface
    // opens on its list of sections and a section opens its detail. Every
    // section stays reachable, and Back returns to the list.
    const wide = window.matchMedia;
    try {
      Object.defineProperty(window, "matchMedia", {
        writable: true,
        value: (query: string) => ({
          matches: true,
          media: query,
          addEventListener: () => undefined,
          removeEventListener: () => undefined,
        }),
      });
      const view = renderApp();
      await waitFor(() => expect(element(view, ".rail__item:nth-of-type(4)")).toBeTruthy());

      // The rail opens the list of sections: the class the narrow viewport
      // reads to hide the sidebar is not on the shell, so the section buttons
      // are the ones on screen.
      flush(() => fireEvent.click(element(view, ".rail__item:nth-of-type(4)")));
      await waitFor(() => expect(view.container.querySelectorAll(".conv")).toHaveLength(4));
      expect(element(view, ".app").className).not.toContain("app--detail");

      // A section opens its detail, and Back returns to the list.
      flush(() => fireEvent.click(button(view, "Computers")));
      await waitFor(() => expect(element(view, ".app").className).toContain("app--detail"));
      expect(panelText(view, "Computers")).toBeTruthy();
      flush(() => fireEvent.click(element(view, ".page__back")));
      await waitFor(() => expect(element(view, ".app").className).not.toContain("app--detail"));
      expect(view.container.querySelectorAll(".conv")).toHaveLength(4);

      // The stylesheet the class is read from hides the sidebar beside a detail,
      // so the class is what decides which of the two a narrow viewport shows.
      const css = readFileSync(resolve(process.cwd(), "src/styles.css"), "utf8");
      const narrow = css.slice(css.indexOf("@media (max-width: 900px)"));
      expect(narrow).toContain(".app--detail .sidebar");
    } finally {
      window.matchMedia = wide;
    }
  });

  it("reaches the Settings sections while the Space still needs setup", async () => {
    // A Space with no Provider yet shows its setup view, which fills the main
    // area. A narrow viewport must still reach the list of sections, because a
    // Provider is connected from the Providers section.
    space.providers = [];
    const wide = window.matchMedia;
    try {
      Object.defineProperty(window, "matchMedia", {
        writable: true,
        value: (query: string) => ({
          matches: true,
          media: query,
          addEventListener: () => undefined,
          removeEventListener: () => undefined,
        }),
      });
      const view = renderApp();
      await waitFor(() => expect(element(view, ".rail__item:nth-of-type(4)")).toBeTruthy());
      // The setup view is what fills the main area while there is no Provider.
      expect(element(view, ".app").className).toContain("app--setup");

      // The rail opens the Settings surface, and its list of sections is the
      // surface on screen.
      flush(() => fireEvent.click(element(view, ".rail__item:nth-of-type(4)")));
      await waitFor(() => expect(view.container.querySelectorAll(".conv")).toHaveLength(4));
      expect(element(view, ".app").className).not.toContain("app--setup");
      expect(element(view, ".app").className).not.toContain("app--detail");
    } finally {
      window.matchMedia = wide;
    }
  });

  it("reaches the Settings sections from a Chat that is already open", async () => {
    // A narrow viewport shows one surface at a time, and the Chat left open
    // behind the Settings surface must not be the one that decides: the rail
    // view does. Otherwise the list of sections is the hidden surface and three
    // of the four sections cannot be reached.
    const wide = window.matchMedia;
    try {
      Object.defineProperty(window, "matchMedia", {
        writable: true,
        value: (query: string) => ({
          matches: true,
          media: query,
          addEventListener: () => undefined,
          removeEventListener: () => undefined,
        }),
      });
      const view = renderApp();
      // A Chat is open, which is what put the shell in its detail layout.
      await waitFor(() => expect(element(view, ".conv")).toBeTruthy());
      flush(() => fireEvent.click(element(view, ".conv")));
      await waitFor(() => expect(element(view, ".chat")).toBeTruthy());
      expect(element(view, ".app").className).toContain("app--detail");

      // The rail opens the Settings surface, and its list of sections is the
      // surface on screen.
      flush(() => fireEvent.click(element(view, ".rail__item:nth-of-type(4)")));
      await waitFor(() => expect(view.container.querySelectorAll(".conv")).toHaveLength(4));
      expect(element(view, ".app").className).not.toContain("app--detail");

      // The Bots surface is reachable the same way.
      flush(() => fireEvent.click(element(view, ".rail__item:nth-of-type(3)")));
      await waitFor(() => expect(element(view, ".conv")).toBeTruthy());
      expect(element(view, ".app").className).not.toContain("app--detail");
    } finally {
      window.matchMedia = wide;
    }
  });

  it("keeps the rail a narrow viewport can reach", () => {
    // The rail is the only way to a management surface, so the narrow layout
    // keeps it: a hidden rail would leave a narrow User no way to the Bots or
    // the Settings. The layout itself is a stylesheet rule, so the rule is what
    // the page holds.
    const css = readFileSync(resolve(process.cwd(), "src/styles.css"), "utf8");
    const narrow = css.slice(css.indexOf("@media (max-width: 900px)"));
    expect(narrow).not.toMatch(/\.rail\s*\{[^}]*display:\s*none/);
  });

  it("keeps the rail's items out of the notch and the home indicator", () => {
    // The rail stays on a narrow viewport, so it is the rail's first and last
    // item that an installed window's notch and home indicator would cover. The
    // layout itself is a stylesheet rule, so the rule is what the page holds.
    const css = readFileSync(resolve(process.cwd(), "src/styles.css"), "utf8");
    const narrow = css.slice(css.indexOf("@media (max-width: 900px)"));
    const rail = narrow.match(/\.rail\s*\{([^}]*)\}/)?.[1] ?? "";
    expect(rail).toMatch(/padding-top:\s*calc\([^;]*env\(safe-area-inset-top/);
    expect(rail).toMatch(/padding-bottom:\s*calc\([^;]*env\(safe-area-inset-bottom/);
  });

  it("keeps a management page's Back out of the notch", () => {
    // A narrow viewport shows one surface at a time, so a management page's head
    // is the only thing over its Back action: an installed window's notch covers
    // it unless the head reserves the inset. The layout itself is a stylesheet
    // rule, so the rule is what the page holds.
    const css = readFileSync(resolve(process.cwd(), "src/styles.css"), "utf8");
    const narrow = css.slice(css.indexOf("@media (max-width: 900px)"));
    expect(narrow).toMatch(
      /\.page__head\s*\{[^}]*padding-top:\s*calc\([^;]*env\(safe-area-inset-top/,
    );
  });

  it("takes a grant away from a Computer that takes no new one", async () => {
    // The daemon on this Computer names no grant it does not have, but the Bot
    // already holds folders from a Computer that did: what is held can always be
    // taken away, because the Space reads a removal as a narrowing.
    space.computer.supports_grants = false;
    space.bot.grants = ["/data/one", "/data/two"];
    const view = await openBots();

    // The folders are listed, each with the way back.
    expect(panelText(view, "Granted folders")).toContain("/data/one");
    expect(view.container.querySelectorAll(".row__remove")).toHaveLength(2);
    // Only the offer of a new one is gone.
    expect(view.container.querySelector('[name="grant_path"]')).toBeNull();

    // Taking one away still writes, and the page reads the Bot again.
    flush(() => fireEvent.click(element(view, ".row__remove")));
    await waitFor(() => expect(view.container.querySelectorAll(".row__remove")).toHaveLength(1));
    expect(space.updates.at(-1)).toMatchObject({ bot: "b1", grant_remove: "/data/one" });
    await drain();
  });

  it("turns full access off on a Computer that cannot run without its sandbox", async () => {
    // The daemon names no full access it does not have, but the Bot has it from
    // a Computer that did: what is on can always be turned off.
    space.computer.supports_full_access = false;
    space.bot.full_access = true;
    const view = await openBots();

    // The switch is there, it is not asking, and it says full access is on.
    const box = field<HTMLInputElement>(view, "full_access");
    expect(box.checked).toBe(true);
    expect(box.disabled).toBe(false);
    expect(panelText(view, "Full access")).toContain("Full access is on.");
    // Turning it on again is not offered: no risk is asked, because the switch
    // only goes off from here.
    expect(view.container.querySelector(".risk")).toBeNull();

    flush(() => fireEvent.click(box));
    // The page reads the Bot again, so the note that said it was on goes.
    await waitFor(() => expect(panelText(view, "Full access")).not.toContain("Full access is on."));
    expect(space.updates.at(-1)).toMatchObject({ bot: "b1", full_access: false });
    expect(space.updates.at(-1)).not.toHaveProperty("full_access_consent");
    await drain();
  });

  it("drops a Model read that answers after the User opened another Bot", async () => {
    space.chats = [space.chat, space.otherChat];
    space.models = [thinker, quiet];
    const view = await openBots();

    // The write goes out, and the read that would bring the Bot back is held.
    space.holdRules("b1");
    choose(view, "bot_model", "m1");
    press(view, "Save Model");
    await waitFor(() => expect(space.updates).toHaveLength(1));
    expect(space.updates[0]).toMatchObject({ bot: "b1", model: "m1" });

    // The User opens another Bot while the answer is still in flight.
    await openOtherBot(view);
    await waitFor(() => expect(element(view, ".page__titles h1").textContent).toBe("Quill"));

    // The held read answers now. It belongs to the Bot the User left, so the
    // page it would write is gone: the page still names the Bot that is open.
    space.releaseRules();
    await drain();
    expect(element(view, ".page__titles h1").textContent).toBe("Quill");
    await drain();
  });

  it("drops a rule read that answers after the User opened another Bot", async () => {
    // Two Bots, one rule each, so a read that lands on the wrong page shows the
    // wrong Bot's rule.
    space.chats = [space.chat, space.otherChat];
    space.rules = [
      askRule(),
      { ...askRule(), rule: "rule2", bot: "b2", tool: "write_file", command_prefix: null },
    ];
    const view = await openBots();
    expect(element(view, ".rows .row__name").textContent).toBe("shell");

    // The write goes out, and the read that would bring the rules back is held.
    space.holdRules("b1");
    flush(() => fireEvent.click(element<HTMLButtonElement>(view, ".row__remove")));
    await waitFor(() => expect(space.ruleRemoves).toEqual(["rule1"]));

    // The User opens another Bot while the answer is still in flight.
    await openOtherBot(view);
    await waitFor(() => expect(element(view, ".page__titles h1").textContent).toBe("Quill"));
    expect(element(view, ".rows .row__name").textContent).toBe("write_file");

    // The held read answers now. It belongs to the Bot the User left, so the
    // page it would write is gone.
    space.releaseRules();
    await drain();
    expect(element(view, ".page__titles h1").textContent).toBe("Quill");
    expect(element(view, ".rows .row__name").textContent).toBe("write_file");
    expect(panelText(view, "Ask and Allow rules")).not.toContain("No rule is set.");
    await drain();
  });

  it("drops a Bot page read that answers after Back closed it", async () => {
    space.bot.grants = ["/data"];
    const view = await openBots();
    expect(panelText(view, "Granted folders")).toContain("/data");

    // The write goes out, and the read that would fill the page back in is held.
    space.holdRules("b1");
    flush(() => fireEvent.click(element(view, ".row__remove")));
    await waitFor(() => expect(space.updates).toHaveLength(1));
    expect(space.updates[0]).toMatchObject({ bot: "b1", grant_remove: "/data" });

    // Back closes the page while the answer is still in flight.
    flush(() => fireEvent.click(element(view, ".page__back")));
    await waitFor(() => expect(view.container.querySelector(".page__body")).toBeNull());

    // The held read answers now. It belongs to the page Back closed, so the
    // detail does not come back to fill the list.
    space.releaseRules();
    await drain();
    expect(view.container.querySelector(".page__body")).toBeNull();
    await drain();
  });

  it("names a rule's tool from the set every Bot has", async () => {
    const view = await openBots();
    const tools = field<HTMLSelectElement>(view, "rule_tool");

    // The tools are the ones the execution side runs, and nothing else.
    expect(Array.from(tools.options).map((option) => option.value)).toEqual([
      "",
      "shell",
      "read_file",
      "write_file",
      "list_files",
    ]);
    // The form stays closed until a tool is chosen.
    expect(button(view, "Add rule").disabled).toBe(true);
    choose(view, "rule_tool", "read_file");
    await waitFor(() => expect(button(view, "Add rule").disabled).toBe(false));

    // A file rule covers a path prefix.
    type(view, "rule_path", "/data");
    press(view, "Add rule");
    await waitFor(() => expect(space.ruleAdds).toHaveLength(1));
    expect(space.ruleAdds[0]).toMatchObject({ tool: "read_file", path_prefix: "/data" });
  });
});
