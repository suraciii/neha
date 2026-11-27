import { cleanup, fireEvent, waitFor } from "@solidjs/testing-library";
import { flush } from "solid-js";
import { describe, expect, it, vi } from "vitest";

import App from "./App";
import { ProtocolError } from "./lib/ws";
import { space } from "./test/fakeSpace";
import { drain, element, renderApp, useView } from "./test/view";

vi.mock("./lib/ws", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./lib/ws")>()),
  Commands: (await import("./test/fakeSpace")).FakeCommands,
}));

type View = ReturnType<typeof renderApp>;

useView();

// The button the test names, by what it says, or none when the page holds none.
function button(view: View, text: string): HTMLButtonElement | null {
  return (
    Array.from(view.container.querySelectorAll<HTMLButtonElement>("button")).find(
      (known) => known.textContent?.trim() === text,
    ) ?? null
  );
}

// The button the test names, which has to be on the page.
function pressed(view: View, text: string): HTMLButtonElement {
  const found = button(view, text);
  if (!found) throw new Error(`no ${text} button in the view`);
  return found;
}

// Every command the client sent the Space under the name the test names, with
// the arguments as they arrived.
function frames(name: string) {
  return space.frames.filter((frame) => frame.name === name);
}

// Puts the text the test names into the field of the same name.
function type(view: View, name: string, text: string) {
  flush();
  fireEvent.input(element<HTMLInputElement>(view, `[name="${name}"]`), { target: { value: text } });
  flush();
}

// Waits until the form's submit answers to the name that was typed.
async function readyToCreate(view: View) {
  await waitFor(() => expect(pressed(view, "Create Bot").disabled).toBe(false));
}

// Opens the Bots list, which the rail reaches and the Settings sections do not
// fill.
async function openBotsList(): Promise<View> {
  const view = renderApp();
  await waitFor(() => expect(element(view, ".rail__item:nth-of-type(3)")).toBeTruthy());
  flush(() => fireEvent.click(element(view, ".rail__item:nth-of-type(3)")));
  // The sidebar fills with the Bots rather than with the Settings sections.
  await waitFor(() => expect(element(view, ".sidebar__title").textContent).toBe("Bots"));
  // The boot's own reads are still in flight when the list opens, so wait for
  // the Chat it opens to be read: a create that follows is then the newest word
  // about which Chat is on screen. A Space with no Bot opens no Chat.
  if (space.chats.length > 0) {
    await waitFor(() => expect(space.asked).toContain("bot_show"));
  }
  return view;
}

// Opens the naming form, with the name the test gives it.
async function openNewBot(view: View, name = "Nova") {
  flush(() => fireEvent.click(pressed(view, "New Bot")));
  if (name !== "") {
    type(view, "bot_name", name);
    await readyToCreate(view);
  }
}

describe("App bots", () => {
  it("offers a new Bot when the list holds none", async () => {
    // A Space with no Bot is where a Bot is needed most, so the action that
    // makes one is on the list that has nothing in it yet.
    space.chats = [];
    const view = await openBotsList();

    expect(element(view, ".side-empty__title").textContent).toBe("No Bots yet");
    expect(pressed(view, "New Bot")).toBeTruthy();

    // The form takes the list's place, and asks for the name alone.
    await openNewBot(view, "");
    expect(element(view, '[name="bot_name"]')).toBeTruthy();
    expect(view.container.querySelector(".side-empty__title")).toBeNull();
    // A name that is not there yet sends nothing.
    expect(pressed(view, "Create Bot").disabled).toBe(true);
  });

  it("makes no Bot where the Space has no Provider", async () => {
    // A Bot needs a Model to work with, and a Model comes from a Provider: the
    // client makes no Bot before one exists.
    space.providers = [];
    space.chats = [];
    const view = await openBotsList();

    // The list says what a Bot is for, and holds no way to make one.
    expect(element(view, ".sidebar__title").textContent).toBe("Bots");
    expect(element(view, ".side-empty__title")).toBeTruthy();
    expect(button(view, "New Bot")).toBeNull();
    // No create is asked for either: the Space is not asked to make one.
    await drain();
    expect(frames("bot_create")).toHaveLength(0);
  });

  it("creates a Bot from the Bots list and opens its Chat", async () => {
    // A Bot is made where the User works, and the Chat the Space opened with it
    // is the one that opens, so the new Bot is usable the moment it exists.
    const view = await openBotsList();
    expect(pressed(view, "New Bot")).toBeTruthy();

    await openNewBot(view);
    flush(() => fireEvent.click(pressed(view, "Create Bot")));

    // The Bot is asked for by its name and nothing else: the Space gives a new
    // Bot a Computer of its own, so the arguments at the boundary are the whole
    // of what was sent.
    await waitFor(() => expect(frames("bot_create")).toEqual([{ name: "bot_create", args: { name: "Nova" } }]));
    expect(space.creates).toEqual([{ name: "Nova" }]);
    // The Space records the create as three events, and the client reads the
    // workspace again for each of them.
    await waitFor(() =>
      expect(space.asked.filter((asked) => asked === "bot_list").length).toBeGreaterThan(3),
    );

    // The Chat the answer names is the one on screen, under the new Bot's name.
    await waitFor(() => expect(element(view, ".chat__titles h1").textContent).toBe("Nova"));
    expect(space.asked).toContain("chat_read");

    // The form is gone, and the list the create was asked from holds the new Bot
    // alongside the one it had.
    expect(view.container.querySelector('[name="bot_name"]')).toBeNull();
    flush(() => fireEvent.click(element(view, ".rail__item:nth-of-type(3)")));
    const names = () =>
      Array.from(view.container.querySelectorAll(".conv__name")).map((row) => row.textContent);
    await waitFor(() => expect(names()).toEqual(["Nova", "Scout"]));
    await drain();
  });

  it("gives a new Bot a Local Computer the User picks", async () => {
    // A Bot may work on a Computer the User already paired, which is the one
    // they choose instead of the fresh Hosted one.
    const view = await openBotsList();
    await openNewBot(view, "");
    flush(() => fireEvent.change(element<HTMLSelectElement>(view, '[name="bot_computer"]'), {
      target: { value: space.computer.id },
    }));
    flush();
    type(view, "bot_name", "Nova");
    await readyToCreate(view);
    flush(() => fireEvent.click(pressed(view, "Create Bot")));

    // The Computer the User picked travels with the name, by its identifier.
    await waitFor(() =>
      expect(frames("bot_create")).toEqual([
        { name: "bot_create", args: { name: "Nova", computer: space.computer.id } },
      ]),
    );
    // The Chat says which Computer the Bot works on.
    await waitFor(() => expect(element(view, ".chat__sub").textContent).toContain("Local Computer"));
    expect(element(view, ".chat__titles h1").textContent).toBe("Nova");
  });

  it("offers no Computer where no Local one is paired", async () => {
    // A Local Computer that is not online is not one a Bot can be given, and a
    // Space with none has nothing to choose: the Bot gets a Hosted Computer.
    space.computer.online = false;
    const view = await openBotsList();
    await openNewBot(view, "");

    expect(view.container.querySelector('[name="bot_computer"]')).toBeNull();
    type(view, "bot_name", "Nova");
    await readyToCreate(view);
    flush(() => fireEvent.click(pressed(view, "Create Bot")));
    await waitFor(() =>
      expect(frames("bot_create")).toEqual([{ name: "bot_create", args: { name: "Nova" } }]),
    );
  });

  it("keeps a Bot it made when the lists cannot be read again", async () => {
    // The Bot is there once the Space has answered. What follows only reads the
    // lists, so a list that cannot be read is never a reason to make a second
    // one: the Bot stays on the screen, and a reload brings the lists back.
    const view = await openBotsList();
    space.refuse.bot_list = new ProtocolError("unavailable", "The Space cannot answer.");
    await openNewBot(view);
    flush(() => fireEvent.click(pressed(view, "Create Bot")));

    // The create was asked for once, and its own answer put the Bot on screen.
    await waitFor(() => expect(element(view, ".chat__titles h1").textContent).toBe("Nova"));
    expect(frames("bot_create")).toHaveLength(1);
    expect(space.creates).toEqual([{ name: "Nova" }]);
    // The form is gone: there is nothing to submit again.
    expect(view.container.querySelector('[name="bot_name"]')).toBeNull();
    expect(view.container.querySelector(".side-form")).toBeNull();

    // A reload reads the Space again, and the Bot is in it.
    space.refuse.bot_list = undefined;
    cleanup();
    const reloaded = renderApp();
    await waitFor(() => expect(element(reloaded, ".rail__item:nth-of-type(3)")).toBeTruthy());
    flush(() => fireEvent.click(element(reloaded, ".rail__item:nth-of-type(3)")));
    await waitFor(() =>
      expect(
        Array.from(reloaded.container.querySelectorAll(".conv__name")).map((row) => row.textContent),
      ).toContain("Nova"),
    );
    await drain();
  });

  it("holds the form still while the create is on its way", async () => {
    // A create the Space has taken cannot be taken back, so the form waits: no
    // second submit, and no way out until the Space has answered.
    const view = await openBotsList();
    await openNewBot(view);
    space.holdCreates();
    flush(() => fireEvent.click(pressed(view, "Create Bot")));

    await waitFor(() => expect(space.asked).toContain("bot_create"));
    await waitFor(() => expect(pressed(view, "Cancel").disabled).toBe(true));
    expect(pressed(view, "Create Bot").disabled).toBe(true);
    // A way out that is not there does nothing.
    flush(() => fireEvent.click(pressed(view, "Cancel")));
    expect(element(view, '[name="bot_name"]')).toBeTruthy();

    space.releaseCreates();
    await waitFor(() => expect(element(view, ".chat__titles h1").textContent).toBe("Nova"));
    expect(frames("bot_create")).toHaveLength(1);
  });

  it("sends nothing for a name that is not there", async () => {
    // An empty name is a Bot the Space would refuse, so the form holds its
    // submit still rather than asking for an answer it cannot have.
    const view = await openBotsList();

    await openNewBot(view, "");
    expect(pressed(view, "Create Bot").disabled).toBe(true);
    flush(() => fireEvent.click(pressed(view, "Create Bot")));
    await drain();

    expect(frames("bot_create")).toHaveLength(0);
    expect(space.creates).toEqual([]);
    // The form stays, with nothing in it, so a name can still be typed.
    expect(element(view, '[name="bot_name"]')).toHaveProperty("value", "");
  });

  it("says what the Space said when it refuses the Bot", async () => {
    // A Bot needs a Model to work with, and a Space with none says so rather
    // than leaving the User to wonder what happened.
    space.refuse.bot_create = new ProtocolError("failed_precondition", "connect a Provider first");
    const view = await openBotsList();

    await openNewBot(view);
    flush(() => fireEvent.click(pressed(view, "Create Bot")));

    // The refusal is named where the Bot was asked for, and the name the User
    // typed is still there to fix.
    await waitFor(() => expect(space.asked).toContain("bot_create"));
    await waitFor(() => expect(element(view, '[role="alert"]')).toBeTruthy());
    expect(element(view, '[name="bot_name"]')).toHaveProperty("value", "Nova");
    expect(pressed(view, "Create Bot")).toBeTruthy();
    // No Chat opens for a Bot that is not there.
    expect(view.container.querySelector(".chat__titles")).toBeNull();
  });

  it("clears the form's own error when it is opened again", async () => {
    // A refusal belongs to the form that asked: opening the form again starts
    // clean, so a name that is not there is not met with the last one's answer.
    space.refuse.bot_create = new ProtocolError("failed_precondition", "connect a Provider first");
    const view = await openBotsList();

    await openNewBot(view);
    flush(() => fireEvent.click(pressed(view, "Create Bot")));
    await waitFor(() => expect(element(view, '[role="alert"]')).toBeTruthy());

    flush(() => fireEvent.click(pressed(view, "Cancel")));
    await openNewBot(view, "");

    expect(view.container.querySelector('[role="alert"]')).toBeNull();
    expect(element(view, '[name="bot_name"]')).toHaveProperty("value", "");
    // And the form may ask again, which the Space now answers.
    space.refuse.bot_create = undefined;
    type(view, "bot_name", "Nova");
    await readyToCreate(view);
    flush(() => fireEvent.click(pressed(view, "Create Bot")));
    // The refusal was asked for once, and the answer that follows is the one the
    // Space gives now.
    await waitFor(() => expect(frames("bot_create")).toHaveLength(2));
    expect(frames("bot_create").at(-1)).toEqual({ name: "bot_create", args: { name: "Nova" } });
  });

  it("closes the form without making a Bot", async () => {
    // A name that is not right yet is not a Bot, so the action is undone rather
    // than left open over the list.
    const view = await openBotsList();

    await openNewBot(view, "");
    flush(() => fireEvent.click(pressed(view, "Cancel")));

    expect(frames("bot_create")).toHaveLength(0);
    expect(view.container.querySelector('[name="bot_name"]')).toBeNull();
    expect(pressed(view, "New Bot")).toBeTruthy();
  });
});
