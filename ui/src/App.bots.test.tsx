import { fireEvent, waitFor } from "@solidjs/testing-library";
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

// The button the test names, by what it says.
function button(view: View, text: string): HTMLButtonElement {
  const found = Array.from(view.container.querySelectorAll<HTMLButtonElement>("button")).find(
    (known) => known.textContent?.trim() === text,
  );
  if (!found) throw new Error(`no ${text} button in the view`);
  return found;
}

// Puts the text the test names into the field of the same name.
function type(view: View, name: string, text: string) {
  flush();
  fireEvent.input(element<HTMLInputElement>(view, `[name="${name}"]`), { target: { value: text } });
  flush();
}

// Waits until the form's submit answers to the name that was typed.
async function readyToCreate(view: View) {
  await waitFor(() => expect(button(view, "Create Bot").disabled).toBe(false));
}

// Opens the Bots list, which the rail reaches and the Settings sections do not
// fill.
async function openBotsList(): Promise<View> {
  const view = renderApp();
  await waitFor(() => expect(element(view, ".rail__item:nth-of-type(3)")).toBeTruthy());
  flush(() => fireEvent.click(element(view, ".rail__item:nth-of-type(3)")));
  // The sidebar fills with the Bots rather than with the Settings sections.
  await waitFor(() => expect(element(view, ".sidebar__title").textContent).toBe("Bots"));
  return view;
}

describe("App bots", () => {
  it("offers a new Bot when the list holds none", async () => {
    // A Space with no Bot is where a Bot is needed most, so the action that
    // makes one is on the list that has nothing in it yet.
    space.chats = [];
    const view = await openBotsList();

    expect(element(view, ".side-empty__title").textContent).toBe("No Bots yet");
    expect(button(view, "New Bot")).toBeTruthy();

    // The form takes the list's place, and asks for the name alone.
    flush(() => fireEvent.click(button(view, "New Bot")));
    expect(element(view, '[name="bot_name"]')).toBeTruthy();
    expect(view.container.querySelector(".side-empty__title")).toBeNull();
    // A name that is not there yet sends nothing.
    expect(button(view, "Create Bot").disabled).toBe(true);
  });

  it("creates a Bot from the Bots list and opens its Chat", async () => {
    // A Bot is made where the User works, and the Chat the Space opened with it
    // is the one that opens, so the new Bot is usable the moment it exists.
    const view = await openBotsList();

    flush(() => fireEvent.click(button(view, "New Bot")));
    type(view, "bot_name", "Nova");
    await readyToCreate(view);
    flush(() => fireEvent.click(button(view, "Create Bot")));

    // The Bot is asked for by its name and nothing else: the Space gives a new
    // Bot its own Computer.
    await waitFor(() => expect(space.creates).toEqual([{ name: "Nova" }]));
    expect(space.asked).toContain("bot_create");

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

  it("sends nothing for a name that is not there", async () => {
    // An empty name is a Bot the Space would refuse, so the form holds its
    // submit still rather than asking for an answer it cannot have.
    const view = await openBotsList();

    flush(() => fireEvent.click(button(view, "New Bot")));
    expect(button(view, "Create Bot").disabled).toBe(true);
    flush(() => fireEvent.click(button(view, "Create Bot")));
    await drain();

    expect(space.asked).not.toContain("bot_create");
    expect(space.creates).toEqual([]);
    // The form stays, with nothing in it, so a name can still be typed.
    expect(element(view, '[name="bot_name"]')).toHaveProperty("value", "");
  });

  it("says what the Space said when it refuses the Bot", async () => {
    // A Bot needs a Model to work with, and a Space with none says so rather
    // than leaving the User to wonder what happened.
    space.refuse.bot_create = new ProtocolError("failed_precondition", "connect a Provider first");
    const view = await openBotsList();

    flush(() => fireEvent.click(button(view, "New Bot")));
    type(view, "bot_name", "Nova");
    await readyToCreate(view);
    flush(() => fireEvent.click(button(view, "Create Bot")));

    // The refusal is named where the Bot was asked for, and the name the User
    // typed is still there to fix.
    await waitFor(() => expect(space.asked).toContain("bot_create"));
    await waitFor(() => expect(element(view, '[role="alert"]')).toBeTruthy());
    expect(element(view, '[name="bot_name"]')).toHaveProperty("value", "Nova");
    expect(button(view, "Create Bot")).toBeTruthy();
    // No Chat opens for a Bot that is not there.
    expect(view.container.querySelector(".chat__titles")).toBeNull();
  });

  it("closes the form without making a Bot", async () => {
    // A name that is not right yet is not a Bot, so the action is undone rather
    // than left open over the list.
    const view = await openBotsList();

    flush(() => fireEvent.click(button(view, "New Bot")));
    flush(() => fireEvent.click(button(view, "Cancel")));

    expect(space.asked).not.toContain("bot_create");
    expect(view.container.querySelector('[name="bot_name"]')).toBeNull();
    expect(button(view, "New Bot")).toBeTruthy();
  });
});
