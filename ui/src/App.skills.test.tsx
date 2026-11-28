// What the rail opens for the Integrations and Skills views, and what those
// views let the reader do against a Space that answers from memory.
import { fireEvent, waitFor } from "@solidjs/testing-library";
import { flush } from "solid-js";
import { describe, expect, it, vi } from "vitest";

import { FakeCommands, bindUrl, signInRefusedReason, signInReplacedReason, space } from "./test/fakeSpace";
import { drain, element, record, renderApp, useView, type View } from "./test/view";

vi.mock("./lib/ws", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./lib/ws")>()),
  Commands: (await import("./test/fakeSpace")).FakeCommands,
}));

useView();

// Presses the rail entry that names itself in the User's language.
function pressRail(view: View, label: string) {
  const entry = [...view.container.querySelectorAll<HTMLElement>(".rail__item")].find(
    (candidate) => candidate.getAttribute("aria-label") === label,
  );
  if (!entry) throw new Error(`no ${label} entry in the rail`);
  flush(() => fireEvent.click(entry));
}

// Presses a compact entry the sidebar shows when the rail is hidden.
function pressSidebar(view: View, label: string) {
  const entry = [...view.container.querySelectorAll<HTMLElement>(".sidebar__views button")].find(
    (candidate) => candidate.textContent?.trim() === label,
  );
  if (!entry) throw new Error(`no ${label} entry in the sidebar`);
  flush(() => fireEvent.click(entry));
}

function press(scope: ParentNode, selector: string, text: string) {
  const target = [...scope.querySelectorAll<HTMLElement>(selector)].find(
    (candidate) => candidate.textContent?.trim() === text,
  );
  if (!target) throw new Error(`no ${text} button in the view`);
  flush(() => fireEvent.click(target));
}

// Types into one field the way the browser reports it.
function type(target: HTMLInputElement | HTMLTextAreaElement, value: string) {
  flush(() => fireEvent.input(target, { target: { value } }));
}

// The fallback link the Integrations view offers when the browser blocked a
// sign-in window, or null when it offers none.
function fallback(view: View): HTMLAnchorElement | null {
  return view.container.querySelector<HTMLAnchorElement>("[data-sign-in-fallback] a");
}

// Fills the add form and starts a browser sign-in from it.
function startSignIn(view: View, name: string, url: string) {
  type(element<HTMLInputElement>(view, 'input[name="server_name"]'), name);
  type(element<HTMLInputElement>(view, 'input[name="server_url"]'), url);
  press(view.container, "button", "Sign in with the server");
}

// The interval handles the page started to poll sign-ins, in start order. The
// test library's own waits poll on a shorter interval, so they are left out.
function signInPollTimers(timers: {
  mock: { calls: unknown[][]; results: { value: unknown }[] };
}): number[] {
  return timers.mock.calls.flatMap((call, index) =>
    call[1] === 1_000 ? [timers.mock.results[index]?.value as number] : [],
  );
}

// Renders the page and waits for the shell a signed-in account sees.
async function boot(): Promise<View> {
  const view = renderApp();
  await waitFor(() => expect(view.container.querySelectorAll(".rail__item")).toHaveLength(4));
  return view;
}

// Opens one library view from the rail and waits until it renders.
async function openView(name: "Integrations" | "Skills"): Promise<View> {
  const view = await boot();
  pressRail(view, name);
  await waitFor(() => expect(element(view, ".library > h1").textContent).toBe(name));
  return view;
}

// The row of one server in the Integrations view.
function serverRow(view: View, server: string): HTMLElement {
  return element<HTMLElement>(view, `[data-server="${server}"]`);
}

// One row's button, by the label it shows.
function rowButton(view: View, server: string, label: string): HTMLButtonElement {
  const found = [...serverRow(view, server).querySelectorAll("button")].find(
    (candidate) => candidate.textContent?.trim() === label,
  );
  if (!found) throw new Error(`no ${label} button in ${server}`);
  return found;
}

// The row of one skill in the Skills view.
function skillRow(view: View, skill: string): HTMLElement {
  return element<HTMLElement>(view, `[data-skill="${skill}"]`);
}

// The checkbox a skill row shows for one Bot.
function botSwitch(row: HTMLElement, bot: string): HTMLInputElement {
  const label = [...row.querySelectorAll("label.chip--button")].find(
    (candidate) => candidate.textContent?.trim() === bot,
  );
  if (!label) throw new Error(`no ${bot} switch in the row`);
  const input = label.querySelector<HTMLInputElement>("input[type=checkbox]");
  if (!input) throw new Error(`no checkbox under ${bot}`);
  return input;
}

describe("App library views", () => {
  it("routes the rail and the compact sidebar entries to each library view", async () => {
    const view = await boot();
    expect(view.container.querySelector(".library")).toBeNull();

    pressRail(view, "Integrations");
    await waitFor(() => expect(element(view, ".library > h1").textContent).toBe("Integrations"));
    // The rail marks the view it opened and the Chat surface gives way to it.
    const integrationsEntry = [...view.container.querySelectorAll(".rail__item")].find(
      (candidate) => candidate.getAttribute("aria-label") === "Integrations",
    );
    expect(integrationsEntry?.getAttribute("aria-current")).toBe("page");
    expect(view.container.querySelector(".chat")).toBeNull();
    await waitFor(() => expect(serverRow(view, "s1")).toBeTruthy());

    pressRail(view, "Skills");
    await waitFor(() => expect(element(view, ".library > h1").textContent).toBe("Skills"));
    await waitFor(() => expect(skillRow(view, "k1")).toBeTruthy());

    // The compact entries in the sidebar open the same views when the rail is
    // hidden; a wide viewport keeps them in the page for a narrow one to show.
    pressSidebar(view, "Integrations");
    await waitFor(() => expect(element(view, ".library > h1").textContent).toBe("Integrations"));

    // A Chat is what the Chats view opens again.
    pressRail(view, "Chats");
    await waitFor(() => expect(element(view, ".chat")).toBeTruthy());
  });

  it("groups the Integrations view by what brought each server", async () => {
    const view = await openView("Integrations");

    await waitFor(() => expect(space.asked).toContain("mcp_list"));
    await waitFor(() => expect(space.asked).toContain("plugin_list"));
    await waitFor(() => expect(serverRow(view, "s1")).toBeTruthy());

    // The Admin reads the packages; a package's servers sit with it.
    const packages = element<HTMLElement>(view, '.library__group[data-group="packages"]');
    expect(packages.textContent).toContain("From packages");
    expect(packages.textContent).toContain("reports");
    expect(packages.querySelector('[data-package="g1"]')).toBeTruthy();
    expect(packages.querySelector('[data-server="s2"]')).toBeTruthy();
    // The same package may bring a stdio server: the page shows its command
    // line instead of an address.
    expect(packages.querySelector('[data-server="s3"]')?.textContent).toContain(
      "stdio ./bin/reporter --data ${PLUGIN_DATA}",
    );
    // A server added directly stays in its own group.
    const direct = element<HTMLElement>(view, '.library__group[data-group="direct"]');
    expect(direct.querySelector('[data-server="s1"]')).toBeTruthy();
    expect(direct.querySelector('[data-server="s2"]')).toBeNull();
  });

  it("enables a package from the Integrations view and reads the change back", async () => {
    space.packages = space.packages.map((pkg) => ({ ...pkg, enabled: false }));
    const view = await openView("Integrations");
    await waitFor(() =>
      expect(element(view, '[data-package="g1"]').textContent).toContain("reports"),
    );
    press(view.container, "button", "Enable");
    await waitFor(() => expect(space.asked).toContain("plugin_enable"));
    // The Space wrote the change, so the row reads it from the list again.
    await waitFor(() =>
      expect(element(view, '[data-package="g1"]').textContent).toContain("Disable"),
    );
  });

  it("keeps the library views reachable on a narrow viewport and offers a way back", async () => {
    const before = window.matchMedia;
    // A viewport at or below the 900px product breakpoint hides the rail.
    Object.defineProperty(window, "matchMedia", {
      writable: true,
      value: (query: string) => ({
        matches: true,
        media: query,
        addEventListener: () => undefined,
        removeEventListener: () => undefined,
      }),
    });
    try {
      const view = await boot();
      pressSidebar(view, "Integrations");
      await waitFor(() => expect(element(view, ".library > h1").textContent).toBe("Integrations"));
      // The rail gives way, so the view carries the way back to the Chats.
      await waitFor(() => expect(element(view, ".library__back")).toBeTruthy());
      flush(() => fireEvent.click(element(view, ".library__back button")));
      await waitFor(() => expect(view.container.querySelector(".library")).toBeNull());
      expect(element(view, ".sidebar__title").textContent).toBe("Chats");
    } finally {
      Object.defineProperty(window, "matchMedia", { writable: true, value: before });
    }
  });

  it("keeps a sign-in window the browser blocked reachable, then reads the list", async () => {
    const view = await openView("Integrations");
    const open = vi.spyOn(window, "open").mockReturnValue(null);
    type(element<HTMLInputElement>(view, 'input[name="server_name"]'), "Docs");
    type(element<HTMLInputElement>(view, 'input[name="server_url"]'), "https://mcp.example.com/docs");
    press(view.container, "button", "Sign in with the server");

    // The blank window opens before the answer, while the click still counts
    // as permission; the Space's answer names where it should go.
    await waitFor(() => expect(open).toHaveBeenCalledWith("about:blank", "_blank"));
    expect(space.asked).toContain("mcp_sign_in_start");
    expect(space.timeouts["mcp_sign_in_start"]).toBe(60_000);

    // The browser blocked it, so the view carries the bind URL as a link.
    await waitFor(() =>
      expect(element(view, "[data-sign-in-fallback] a").getAttribute("href")).toBe(
        bindUrl("i1"),
      ),
    );

    // Polling goes on: a finished sign-in writes the new server to the record,
    // so the list reads it back without the reader leaving the view.
    space.signInState = "finished";
    await waitFor(() => expect(space.asked).toContain("mcp_sign_in_status"), { timeout: 4000 });
    // The Space records the signed-in server beside the fixture servers, so
    // the row the page shows is the one its answer named.
    await waitFor(() => expect(space.servers.some((server) => server.name === "Docs")).toBe(true));
    const added = space.servers.find((server) => server.name === "Docs");
    await waitFor(() => expect(serverRow(view, added!.server)).toBeTruthy(), { timeout: 4000 });
    // The link goes with the sign-in it belonged to.
    await waitFor(() =>
      expect(view.container.querySelector("[data-sign-in-fallback]")).toBeNull(),
    );
  });

  it("opens a sign-in in the window it opened before the answer", async () => {
    const view = await openView("Integrations");
    const popup = {
      closed: false,
      opener: {} as unknown,
      location: { href: "" },
      close: vi.fn(),
    };
    const open = vi.spyOn(window, "open").mockReturnValue(popup as unknown as Window);
    type(element<HTMLInputElement>(view, 'input[name="server_name"]'), "Docs");
    type(element<HTMLInputElement>(view, 'input[name="server_url"]'), "https://mcp.example.com/docs");
    press(view.container, "button", "Sign in with the server");

    await waitFor(() => expect(open).toHaveBeenCalledWith("about:blank", "_blank"));
    // The window the click opened takes the URL the answer named, which is
    // first-party: the Space's bind route, never the provider.
    await waitFor(() => expect(popup.location.href).toBe(bindUrl("i1")));
    expect(new URL(popup.location.href).origin).toBe(window.location.origin);
    expect(popup.opener).toBeNull();
    expect(view.container.querySelector("[data-sign-in-fallback]")).toBeNull();

    space.signInState = "finished";
    await waitFor(() => expect(space.asked).toContain("mcp_sign_in_status"), { timeout: 4000 });
  });

  it("passes the selected scope to a new server's sign-in", async () => {
    const view = await openView("Integrations");
    vi.spyOn(window, "open").mockReturnValue(null);
    type(element<HTMLInputElement>(view, 'input[name="server_name"]'), "Docs");
    type(element<HTMLInputElement>(view, 'input[name="server_url"]'), "https://mcp.example.com/docs");
    flush(() =>
      fireEvent.change(element<HTMLSelectElement>(view, 'select[name="server_scope"]'), {
        target: { value: "user" },
      }),
    );
    press(view.container, "button", "Sign in with the server");

    await waitFor(() => expect(space.asked).toContain("mcp_sign_in_start"));
    const started = space.sent.at(-1) as Record<string, unknown>;
    expect(started).toEqual({
      name: "Docs",
      url: "https://mcp.example.com/docs",
      scope: "user",
    });
    // A browser flow lets the Space name its own callback; the page never
    // supplies a redirect URI.
    expect(started).not.toHaveProperty("redirect_uri");
  });

  it("passes the selected scope to an existing server's sign-in", async () => {
    const view = await openView("Integrations");
    await waitFor(() => expect(serverRow(view, "s2")).toBeTruthy());
    vi.spyOn(window, "open").mockReturnValue(null);
    // The Admin's form names the Space by default: the Space decides whether
    // that scope is allowed for this server.
    await waitFor(() => expect(rowButton(view, "s2", "Sign in").disabled).toBe(false));
    flush(() => fireEvent.click(rowButton(view, "s2", "Sign in")));
    await waitFor(() => expect(space.asked).toContain("mcp_sign_in_start"));
    expect(space.sent.at(-1)).toEqual({ server: "s2", scope: "space" });
  });

  it("fails the first sign-in when a second starts and lets the second finish", async () => {
    const view = await openView("Integrations");
    // The browser blocks both windows, so each sign-in keeps a fallback link
    // while the page polls it.
    vi.spyOn(window, "open").mockReturnValue(null);
    const starts = vi.spyOn(window, "setInterval");
    const stops = vi.spyOn(window, "clearInterval");

    startSignIn(view, "Docs A", "https://mcp.example.com/a");
    await waitFor(() => expect(fallback(view)?.getAttribute("href")).toBe(bindUrl("i1")));

    // A second sign-in starts while the first still polls. The Space keeps one
    // browser flow per account, so the first's next poll reads the fixed reason
    // a newer start replaced it.
    startSignIn(view, "Docs B", "https://mcp.example.com/b");
    await waitFor(() => expect(fallback(view)?.getAttribute("href")).toBe(bindUrl("i2")));
    const timers = signInPollTimers(starts);
    expect(timers).toHaveLength(2);
    const firstTimer = timers[0] as number;
    const secondTimer = timers[1] as number;

    await waitFor(
      () =>
        expect(element(view, ".library .banner--error").textContent).toContain(
          signInReplacedReason,
        ),
      { timeout: 4_000 },
    );

    // The failed flow keeps no pending state: it stopped polling, and the only
    // fallback link on the page is the second flow's.
    expect(stops).toHaveBeenCalledWith(firstTimer);
    expect(stops).not.toHaveBeenCalledWith(secondTimer);
    expect(fallback(view)?.getAttribute("href")).toBe(bindUrl("i2"));

    // The second flow's poll finishes its sign-in normally: the new server
    // enters the list and the pending link goes with the finished flow.
    space.signInState = "finished";
    await waitFor(
      () => expect(space.servers.some((server) => server.name === "Docs B")).toBe(true),
      { timeout: 4_000 },
    );
    await waitFor(() => expect(fallback(view)).toBeNull());
    const added = space.servers.find((server) => server.name === "Docs B");
    await waitFor(() => expect(serverRow(view, added!.server)).toBeTruthy());
    expect(stops).toHaveBeenCalledWith(secondTimer);
  });

  it("clears a failed sign-in's pending link and poll", async () => {
    const view = await openView("Integrations");
    vi.spyOn(window, "open").mockReturnValue(null);
    const starts = vi.spyOn(window, "setInterval");
    const stops = vi.spyOn(window, "clearInterval");

    startSignIn(view, "Docs", "https://mcp.example.com/docs");
    await waitFor(() => expect(fallback(view)?.getAttribute("href")).toBe(bindUrl("i1")));
    const timers = signInPollTimers(starts);
    expect(timers).toHaveLength(1);
    const timer = timers[0] as number;

    // The Space settles the sign-in as failed. The view shows the fixed reason
    // that arrived, drops the pending fallback link, and stops polling it.
    space.signInState = "failed";
    await waitFor(
      () =>
        expect(element(view, ".library .banner--error").textContent).toContain(
          signInRefusedReason,
        ),
      { timeout: 4_000 },
    );
    expect(fallback(view)).toBeNull();
    expect(stops).toHaveBeenCalledWith(timer);
  });

  it("groups the Skills view by library and toggles a skill for a Bot", async () => {
    // Two Bots, so the Space's skill offers a switch for each.
    space.chats = [space.chat, space.otherChat];
    // A skill with scripts, so the row says how many it carries.
    space.skills = [
      ...space.skills,
      {
        id: "k2",
        name: "Release notes",
        description: "Writes the release notes.",
        body: "Write the notes.",
        scripts: [
          { name: "collect.sh", sha256: "sha256:1" },
          { name: "publish.sh", sha256: "sha256:2" },
        ],
        library: "space",
        bot: null,
        plugin: null,
      },
    ];
    const view = await openView("Skills");

    // The Space's library and each owned Bot's library each have a group.
    const room = element<HTMLElement>(view, '.library__group[data-group="space"]');
    expect(room.textContent).toContain("Weekly report");
    expect(room.textContent).toContain("Writes the week's report from the record.");
    expect(element(view, '.library__group[data-group="b1"]').textContent).toContain("Scout");
    expect(element(view, '.library__group[data-group="b2"]').textContent).toContain("Quill");

    // A skill a package brought names the package.
    await waitFor(() => expect(skillRow(view, "k1").textContent).toContain("reports"));
    // The scripts count and their file names come from the Space's list.
    const scripts = skillRow(view, "k2");
    expect(scripts.textContent).toContain("Scripts");
    expect(scripts.textContent).toContain("collect.sh");

    // Scout already uses the Space's skill; Quill does not. Turning it on for
    // Quill goes through the Space and the switch then reads the new list.
    await waitFor(() => expect(botSwitch(skillRow(view, "k1"), "Scout").checked).toBe(true));
    const quill = botSwitch(skillRow(view, "k1"), "Quill");
    expect(quill.checked).toBe(false);
    // The controls wait for a read in flight, so a click only lands once the
    // page has finished reading the lists the last command changed.
    await waitFor(() => expect(botSwitch(skillRow(view, "k1"), "Quill").disabled).toBe(false));
    flush(() => fireEvent.click(botSwitch(skillRow(view, "k1"), "Quill")));
    await waitFor(() => expect(space.asked).toContain("bot_skill_enable"));
    await waitFor(() => expect(botSwitch(skillRow(view, "k1"), "Quill").checked).toBe(true));

    // Turning it off asks for the other command.
    await waitFor(() => expect(botSwitch(skillRow(view, "k1"), "Scout").disabled).toBe(false));
    flush(() => fireEvent.click(botSwitch(skillRow(view, "k1"), "Scout")));
    await waitFor(() => expect(space.asked).toContain("bot_skill_disable"));
  });

  it("admits a member to a Bot's library but not to the Space's", async () => {
    space.account.is_admin = false;
    space.skills = [
      ...space.skills,
      {
        id: "k2",
        name: "Private notes",
        description: "Keeps notes for its own Bot.",
        body: "Write the notes.",
        scripts: [],
        library: "bot",
        bot: "b1",
        plugin: null,
      },
    ];
    try {
      const view = await openView("Skills");
      await waitFor(() => expect(skillRow(view, "k1")).toBeTruthy());
      await waitFor(() => expect(skillRow(view, "k2")).toBeTruthy());

      // A member never asks for the packages the Admin installed.
      expect(space.asked).not.toContain("plugin_list");
      // The Space's library is listed but offers no removal; the Bot's own
      // library is the member's to remove from.
      expect(skillRow(view, "k1").querySelector(".library__actions")?.textContent).not.toContain(
        "Remove",
      );
      expect(skillRow(view, "k2").querySelector(".library__actions")?.textContent).toContain(
        "Remove",
      );
      // The Add form offers the member's Bots, never the Space's library.
      const select = element<HTMLSelectElement>(view, 'select[name="skill_library"]');
      expect([...select.options].map((option) => option.value)).toEqual(["b1"]);

      // Adding a skill goes to the Bot's library, named by the form.
      type(element<HTMLInputElement>(view, 'input[name="skill_name"]'), "daily-notes");
      type(element<HTMLInputElement>(view, 'input[name="skill_description"]'), "Keeps a daily note.");
      flush(() => fireEvent.submit(element<HTMLFormElement>(view, 'form[data-form="skill-add"]')));
      await waitFor(() => expect(space.asked).toContain("skill_add"));
      const added = space.sent.at(-1) as {
        library: string;
        bot?: string;
        document: string;
      };
      expect(added.library).toBe("bot");
      expect(added.bot).toBe("b1");
      expect(added.document).toContain("daily-notes");
      expect(added.document).toContain("Keeps a daily note.");
    } finally {
      space.account.is_admin = true;
    }
  });

  it("adds a Space skill with its scripts from the Admin's form", async () => {
    const view = await openView("Skills");
    await waitFor(() => expect(space.asked).toContain("skill_list"));

    type(element<HTMLInputElement>(view, 'input[name="skill_name"]'), "weekly-digest");
    type(element<HTMLInputElement>(view, 'input[name="skill_description"]'), "Summarizes the week.");
    type(
      element<HTMLTextAreaElement>(view, 'textarea[name="skill_body"]'),
      "1. Read the week.\n2. Write one page.",
    );
    press(view.container, ".link-btn", "Add a script");
    type(element<HTMLInputElement>(view, 'input[name="script_name_0"]'), "collect.sh");
    type(
      element<HTMLTextAreaElement>(view, 'textarea[name="script_content_0"]'),
      "printf week\nprintf done",
    );
    expect(element<HTMLSelectElement>(view, 'select[name="skill_library"]').value).toBe("space");
    flush(() => fireEvent.submit(element<HTMLFormElement>(view, 'form[data-form="skill-add"]')));

    await waitFor(() => expect(space.asked).toContain("skill_add"));
    const added = space.sent.at(-1) as {
      library: string;
      document: string;
      scripts: { name: string; content: string }[];
    };
    expect(added.library).toBe("space");
    expect(added.document).toContain('name: "weekly-digest"');
    expect(added.document).toContain('description: "Summarizes the week."');
    expect(added.document).toContain("2. Write one page.");
    // The script keeps the line break the reader typed: the command carries it
    // exactly as the form holds it.
    expect(added.scripts).toEqual([
      { name: "collect.sh", content: "printf week\nprintf done" },
    ]);
  });

  it("reads a library event again but leaves an unrelated event alone", async () => {
    const view = await openView("Integrations");
    await waitFor(() => expect(serverRow(view, "s1")).toBeTruthy());
    const reads = () => space.asked.filter((name) => name === "mcp_list").length;
    const before = reads();

    // A Message changes no library, so the page does not read the servers.
    record("neha.message.added", { chat: "h2", text: "hello" });
    await drain();
    expect(reads()).toBe(before);

    // A record event about an MCP server is the page's word that the list
    // changed, wherever the change came from.
    space.servers = [
      ...space.servers,
      {
        server: "s4",
        name: "Docs",
        transport: "streamable-http",
        url: "https://mcp.example.com/docs",
        scope: "space",
        user: null,
        by: "u1",
        by_name: "Ada",
        connected: true,
        credential: "space",
      },
    ];
    record("neha.mcp.server.added", { server: "s4", name: "Docs" });
    await waitFor(() => expect(serverRow(view, "s4")).toBeTruthy());
    expect(reads()).toBe(before + 1);
  });

  it("drops the rows of a removed package for a member reading the libraries", async () => {
    space.account.is_admin = false;
    try {
      const view = await boot();
      pressRail(view, "Integrations");
      await waitFor(() => expect(element(view, ".library > h1").textContent).toBe("Integrations"));
      await waitFor(() => expect(serverRow(view, "s2")).toBeTruthy());

      // The member opens the Skills view too, so both lists are loaded. A
      // member never reads the packages themselves.
      pressRail(view, "Skills");
      await waitFor(() => expect(element(view, ".library > h1").textContent).toBe("Skills"));
      await waitFor(() => expect(skillRow(view, "k1")).toBeTruthy());
      pressRail(view, "Integrations");
      await waitFor(() => expect(serverRow(view, "s2")).toBeTruthy());
      expect(space.asked).not.toContain("plugin_list");

      const reads = (name: string) => space.asked.filter((asked) => asked === name).length;
      const serverReads = reads("mcp_list");
      const skillReads = reads("skill_list");

      // The Space removed the package and told every account. The member's open
      // lists read again from mcp_list and skill_list, and the rows the package
      // brought leave both views.
      space.removePackage("g1");
      await waitFor(() =>
        expect(view.container.querySelector('[data-server="s2"]')).toBeNull(),
      );
      await waitFor(() => expect(reads("mcp_list")).toBe(serverReads + 1));
      await waitFor(() => expect(reads("skill_list")).toBe(skillReads + 1));
      expect(view.container.querySelector('[data-package="g1"]')).toBeNull();

      pressRail(view, "Skills");
      await waitFor(() =>
        expect(view.container.querySelector('[data-skill="k1"]')).toBeNull(),
      );
      expect(space.asked).not.toContain("plugin_list");
    } finally {
      space.account.is_admin = true;
    }
  });

  it("shows a refused command as one alert and keeps the list", async () => {
    const view = await openView("Integrations");
    await waitFor(() => expect(serverRow(view, "s1")).toBeTruthy());
    const command = FakeCommands.prototype.command;
    vi.spyOn(FakeCommands.prototype, "command").mockImplementation(function (
      this: FakeCommands,
      name: string,
      args?: unknown,
    ) {
      if (name === "mcp_remove") return Promise.reject(new Error("the server is in use"));
      return command.call(this, name, args);
    });

    press(serverRow(view, "s1"), "button", "Remove");
    await waitFor(() =>
      expect(element(view, ".library .banner--error").textContent).toContain(
        "The Space cannot complete this request.",
      ),
    );
    // The record did not change, so the row the page showed stays.
    expect(serverRow(view, "s1")).toBeTruthy();
    expect(space.servers.some((server) => server.server === "s1")).toBe(true);
  });

  it("lets a member use a server a package brought", async () => {
    space.account.is_admin = false;
    vi.spyOn(window, "open").mockReturnValue(null);
    try {
      const view = await openView("Integrations");
      await waitFor(() => expect(serverRow(view, "s2")).toBeTruthy());

      // A member never reads the Admin's package list, but the servers the
      // package brought stay grouped and usable under a neutral name.
      expect(space.asked).not.toContain("plugin_list");
      const packages = element<HTMLElement>(view, '.library__group[data-group="packages"]');
      expect(packages.textContent).toContain("From packages");
      expect(
        element<HTMLElement>(view, '[data-package="g1"]').textContent,
      ).toContain("Installed package");
      expect(packages.querySelector('[data-server="s2"]')).toBeTruthy();

      // The member connects it with their own credential.
      await waitFor(() => expect(rowButton(view, "s2", "Connect").disabled).toBe(false));
      flush(() => fireEvent.click(rowButton(view, "s2", "Connect")));
      type(
        element<HTMLInputElement>(view, '[data-server="s2"] input[name="connect_token"]'),
        "member-token",
      );
      flush(() => fireEvent.submit(element<HTMLFormElement>(view, '[data-server="s2"] form')));
      await waitFor(() => expect(space.asked).toContain("mcp_connect"));
      const connected = space.sent.at(-1) as { server: string; credential: string };
      expect(connected).toEqual({ server: "s2", credential: "member-token" });
      await waitFor(() =>
        expect(space.servers.find((server) => server.server === "s2")?.connected).toBe(true),
      );

      // And enables it for a Bot they own.
      await waitFor(() =>
        expect(
          botSwitch(serverRow(view, "s2"), "Scout").disabled,
        ).toBe(false),
      );
      flush(() => fireEvent.click(botSwitch(serverRow(view, "s2"), "Scout")));
      await waitFor(() => expect(space.asked).toContain("bot_mcp_enable"));
      expect(space.bot.mcps).toContain("s2");
    } finally {
      space.account.is_admin = true;
    }
  });

  it("says the Integrations list is loading until the Space answers", async () => {
    const view = await boot();
    space.holdLists();
    try {
      pressRail(view, "Integrations");
      await waitFor(() => expect(element(view, ".library > h1").textContent).toBe("Integrations"));
      // The empty state only speaks for a completed load.
      expect(view.container.textContent).toContain("Loading…");
      expect(view.container.textContent).not.toContain("No integrations yet");
    } finally {
      space.releaseLists();
    }
    await waitFor(() => expect(serverRow(view, "s1")).toBeTruthy());
    expect(view.container.textContent).not.toContain("Loading…");
  });

  it("keeps a library's empty state until its load finishes", async () => {
    space.skills = [];
    const view = await boot();
    space.holdLists();
    try {
      pressRail(view, "Skills");
      await waitFor(() => expect(element(view, ".library > h1").textContent).toBe("Skills"));
      expect(view.container.textContent).toContain("Loading…");
      expect(view.container.textContent).not.toContain("The Space holds no skills.");
    } finally {
      space.releaseLists();
    }
    await waitFor(() =>
      expect(view.container.textContent).toContain("The Space holds no skills."),
    );
  });

  it("closes an inline credential form when the server list changes", async () => {
    const view = await openView("Integrations");
    await waitFor(() => expect(serverRow(view, "s1")).toBeTruthy());
    await waitFor(() => expect(rowButton(view, "s1", "Connect").disabled).toBe(false));
    flush(() => fireEvent.click(rowButton(view, "s1", "Connect")));
    type(
      element<HTMLInputElement>(view, '[data-server="s1"] input[name="connect_token"]'),
      "stale-secret",
    );

    // The record says the list changed, so the page reads it again and the
    // form opened against the old list closes.
    record("neha.mcp.server.added", { server: "s9", name: "Docs" });
    await waitFor(() =>
      expect(
        view.container.querySelector('[data-server="s1"] input[name="connect_token"]'),
      ).toBeNull(),
    );
    expect(view.container.innerHTML).not.toContain("stale-secret");
  });
});
