import { fireEvent, waitFor } from "@solidjs/testing-library";
import { flush } from "solid-js";
import { describe, expect, it, vi } from "vitest";

import App from "./App";
import { ProtocolError } from "./lib/ws";
import { space } from "./test/fakeSpace";
import { drain, element, record, renderApp, useView } from "./test/view";
import type { Model, Rule } from "./types";

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

// The button the test names, found by what it says.
function button(view: View, text: string): HTMLButtonElement {
  const found = Array.from(view.container.querySelectorAll<HTMLButtonElement>("button")).find(
    (known) => known.textContent?.trim() === text,
  );
  if (!found) throw new Error(`no ${text} button in the view`);
  return found;
}

// The field the test names, by its name.
function field<T extends HTMLElement>(view: View, name: string): T {
  return element<T>(view, `[name="${name}"]`);
}

// What one panel says, found by its own heading.
function panelText(view: View, heading: string): string {
  const panel = Array.from(view.container.querySelectorAll(".panel")).find(
    (known) => known.querySelector("h2")?.textContent === heading,
  );
  if (!panel) throw new Error(`no ${heading} panel in the view`);
  return panel.textContent ?? "";
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
async function openBots(): Promise<View> {
  const view = renderApp();
  await waitFor(() => expect(element(view, ".rail__item:nth-of-type(3)")).toBeTruthy());
  flush(() => fireEvent.click(element(view, ".rail__item:nth-of-type(3)")));
  // The sidebar lists every Bot the Space knows.
  await waitFor(() => expect(view.container.querySelectorAll(".conv")).toHaveLength(1));
  flush(() => fireEvent.click(element(view, ".conv")));
  await waitFor(() => expect(element(view, ".page__body")).toBeTruthy());
  return view;
}

// Opens the Settings surface on the section the test names: Providers, Models,
// or the catalog.
async function openSettings(section = 1): Promise<View> {
  const view = renderApp();
  await waitFor(() => expect(element(view, ".rail__item:nth-of-type(4)")).toBeTruthy());
  flush(() => fireEvent.click(element(view, ".rail__item:nth-of-type(4)")));
  await waitFor(() => expect(view.container.querySelectorAll(".conv")).toHaveLength(3));
  flush(() => fireEvent.click(element(view, `.conv:nth-of-type(${section})`)));
  await waitFor(() => expect(element(view, ".page__body")).toBeTruthy());
  return view;
}

// The rule the Bot holds, which a test reads back.
function askRule(): Rule {
  return {
    rule: "rule1",
    bot: "b1",
    user: "u1",
    effect: "ask",
    tool: "shell",
    command_prefix: "git",
    path_prefix: null,
  };
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

    // A Model that thinks names the levels it accepts.
    choose(view, "bot_model", "m1");
    await waitFor(() => expect(field<HTMLSelectElement>(view, "bot_thinking").options).toHaveLength(3));
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

    // The second Model thinks less: it accepts no "high", so the Bot keeps the
    // nearest level it does accept.
    choose(view, "bot_model", "m2");
    await waitFor(() => expect(field<HTMLSelectElement>(view, "bot_thinking").value).toBe("medium"));
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
    await waitFor(() => expect(panelText(view, "Granted folders")).toContain("No folder is granted."));

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

    // The switch arms the risk first; it writes nothing yet.
    flush(() => fireEvent.click(fullAccess));
    expect(space.updates).toHaveLength(0);
    await waitFor(() => expect(element(view, ".risk__text").textContent).toContain("read and change any file"));

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
    expect(panelText(view, "Granted folders")).toContain("A Hosted Computer takes no granted folder.");
    expect(panelText(view, "Full access")).toContain("full access does not apply");
    expect(view.container.querySelector('[name="full_access"]')).toBeNull();

    press(view, "Reset Workspace");
    expect(space.resets).toHaveLength(0);
    await waitFor(() => expect(button(view, "Press again to reset").className).toContain("btn--danger"));

    press(view, "Press again to reset");
    await waitFor(() => expect(space.resets).toEqual(["p1"]));
  });

  it("moves a Bot to another paired Computer", async () => {
    const view = await openBots();
    expect(panelText(view, "Computer")).toContain("/w");

    // The other paired Local Computer is where a Bot may move.
    const target = field<HTMLSelectElement>(view, "move_target");
    expect(target.options).toHaveLength(2);
    expect(target.options[1]?.textContent).toBe("/other");

    choose(view, "move_target", "p9");
    press(view, "Move to another Computer");
    // The first press only arms it.
    expect(space.moves).toHaveLength(0);
    await waitFor(() => expect(button(view, "Press again to move")).toBeTruthy());

    press(view, "Press again to move");
    await waitFor(() => expect(space.moves).toEqual([{ bot: "b1", computer: "p9" }]));
    await waitFor(() => expect(panelText(view, "Computer")).toContain("/other"));
    // The page names the new Computer offline, as the Space knows it.
    expect(panelText(view, "Computer")).toContain("Offline");
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
    await waitFor(() => expect(panelText(view, "Ask and Allow rules")).toContain("No rule is set."));

    // A new rule names its effect, its tool, and what it covers.
    choose(view, "rule_effect", "ask");
    type(view, "rule_tool", "write_file");
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

  it("says why the Space refused a change", async () => {
    space.refuse.bot_update = new ProtocolError("forbidden", "The Space does not allow that.");
    const view = await openBots();

    type(view, "bot_name", "Nova");
    press(view, "Save name");

    // The page names the refusal where the change was asked for, and keeps the
    // Bot's own name.
    await waitFor(() => expect(space.asked).toContain("bot_update"));
    await waitFor(() => expect(element(view, '[role="alert"]').textContent).toBe(
      "This Space does not allow that change.",
    ));
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
    await waitFor(() => expect(element(view, ".row__name").textContent).toBe("https://api.openai.com/v1"));
    await waitFor(() => expect(field<HTMLInputElement>(view, "api_key").value).toBe(""));
    await waitFor(() => expect(element(view, ".page__note").textContent).toBe("Models refreshed"));
  });

  it("makes one Model the default every Bot starts from", async () => {
    space.models = [
      thinker,
      { ...quiet, tools: true, is_default: false, thinking_levels: ["low"] },
    ];
    const view = await openSettings(2);

    // The default Model says so, and the others offer the change.
    const rows = view.container.querySelectorAll(".row");
    expect(rows[0]?.textContent).toContain("Space default");
    expect(rows[1]?.textContent).toContain("Make default");
    expect(rows[0]?.textContent).toContain("Context 400000");

    press(view, "Make default");
    await waitFor(() => expect(space.defaults).toEqual(["m2"]));
    await waitFor(() => expect(element(view, ".page__note").textContent).toBe("Default Model saved"));
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
      model_count: 900,
    };
    const view = await openSettings(3);

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
      model_count: 40,
    };
    const view = await openSettings(3);

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
    await waitFor(() => expect(element(view, ".side-empty__title").textContent).toBe("No Provider yet"));

    // So are the Models, which come with them.
    flush(() => fireEvent.click(element(view, ".conv:nth-of-type(2)")));
    await waitFor(() => expect(element(view, ".side-empty__title").textContent).toBe("No Model yet"));

    // A Space with no Bot has no Bot page to open.
    flush(() => fireEvent.click(element(view, ".rail__item:nth-of-type(3)")));
    await waitFor(() => expect(element(view, ".side-empty__title").textContent).toBe("No Bots yet"));
    expect(view.container.querySelector(".page__body")).toBeNull();
  });

  it("names a management change the record reports", async () => {
    const view = await openBots();
    // Another client renamed the Bot, and the page shows its name.
    space.bot.name = "Renamed";
    record("neha.bot.updated", { bot: "b1", name: "Renamed" });
    await waitFor(() => expect(element(view, ".page__titles h1").textContent).toBe("Renamed"));
    await drain();
  });
});
