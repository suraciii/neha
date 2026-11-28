import { cleanup, fireEvent, render, waitFor } from "@solidjs/testing-library";
import { flush } from "solid-js";
import { afterEach, describe, expect, it, vi } from "vitest";

import IntegrationsView, { type IntegrationsViewProps } from "./components/IntegrationsView";
import { setLocale } from "./i18n";
import { FakeCommands } from "./test/fakeSpace";
import { element } from "./test/view";
import type { Bot, InstalledPackage, McpServer } from "./types";

const admin = { id: "u1", displayName: "Ada", isAdmin: true };
const member = { id: "u2", displayName: "Bo", isAdmin: false };

const bots: Bot[] = [
  { id: "b1", owner: "u1", name: "Scout", computer: "p1", chat: "h1", mcps: ["s1"] },
  { id: "b2", owner: "u1", name: "Quill", computer: "p1", chat: "h2" },
];

// The Space's server, connected with the Space's own credential.
const searchServer: McpServer = {
  server: "s1",
  name: "Search",
  url: "https://mcp.example.com/search",
  scope: "space",
  user: null,
  by: "u1",
  by_name: "Ada",
  connected: true,
  credential: "space",
};

// The server a package brought, which nobody has connected yet.
const reportServer: McpServer = {
  server: "s2",
  name: "Reports",
  url: "https://mcp.example.com/reports",
  scope: "space",
  user: null,
  by: "u1",
  by_name: "Ada",
  plugin: "g1",
  connected: false,
  credential: "none",
};

const installedPackage: InstalledPackage = {
  plugin: "g1",
  name: "reports",
  version: "1.2.0",
  commit: "0123456789abcdef",
  digest: "sha256:0",
  source: "https://github.com/example/reports",
  enabled: true,
  servers: [{ server: "s2", name: "Reports" }],
  skills: [{ skill: "k1", name: "Weekly report" }],
};

type View = ReturnType<typeof render>;

function renderView(overrides: Partial<IntegrationsViewProps> = {}) {
  const onAdd = vi.fn(() => Promise.resolve());
  const onConnect = vi.fn(() => Promise.resolve());
  const onSignIn = vi.fn(() => Promise.resolve());
  const onRemove = vi.fn(() => Promise.resolve());
  const onBot = vi.fn(() => Promise.resolve());
  const onPackageEnabled = vi.fn(() => Promise.resolve());
  const onScopeChange = vi.fn();
  const props: IntegrationsViewProps = {
    account: admin,
    bots,
    servers: [searchServer, reportServer],
    packages: [installedPackage],
    busy: false,
    error: null,
    onAdd,
    onConnect,
    onSignIn,
    onRemove,
    onBot,
    onPackageEnabled,
    onScopeChange,
    ...overrides,
  };
  const view = render(() => <IntegrationsView {...props} />);
  return {
    view,
    onAdd,
    onConnect,
    onSignIn,
    onRemove,
    onBot,
    onPackageEnabled,
    onScopeChange,
  };
}

// Solid applies an event's state changes when the event settles, so every
// event a test follows with another action or a read flushes first.
function type(input: HTMLInputElement | HTMLTextAreaElement, value: string) {
  flush(() => fireEvent.input(input, { target: { value } }));
}

function choose(select: HTMLSelectElement, value: string) {
  flush(() => fireEvent.change(select, { target: { value } }));
}

function submit(form: HTMLFormElement) {
  flush(() => fireEvent.submit(form));
}

function click(button: HTMLElement) {
  flush(() => fireEvent.click(button));
}

function group(view: View, kind: "packages" | "direct"): HTMLElement {
  return element<HTMLElement>(view, `.library__group[data-group="${kind}"]`);
}

function row(view: View, server: string): HTMLElement {
  return element<HTMLElement>(view, `[data-server="${server}"]`);
}

function packageRow(view: View, plugin: string): HTMLElement {
  return element<HTMLElement>(view, `[data-package="${plugin}"]`);
}

function button(within: HTMLElement, label: string): HTMLButtonElement {
  const found = [...within.querySelectorAll("button")].find(
    (candidate) => candidate.textContent?.trim() === label,
  );
  if (!found) throw new Error(`no ${label} button in the row`);
  return found;
}

function input(within: HTMLElement, selector: string): HTMLInputElement {
  const found = within.querySelector<HTMLInputElement>(selector);
  if (!found) throw new Error(`no ${selector} in the row`);
  return found;
}

function checkbox(within: HTMLElement, bot: string): HTMLInputElement {
  const label = [...within.querySelectorAll("label.chip--button")].find(
    (candidate) => candidate.textContent?.trim() === bot,
  );
  if (!label) throw new Error(`no ${bot} chip in the row`);
  return input(label as HTMLElement, "input[type=checkbox]");
}

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  setLocale("en-US");
});

describe("IntegrationsView", () => {
  it("groups installed packages apart from the servers added directly", () => {
    const { view } = renderView();
    const packagesGroup = group(view, "packages");
    const directGroup = group(view, "direct");

    expect(packagesGroup.textContent).toContain("From packages");
    expect(directGroup.textContent).toContain("Added directly");

    const installed = packageRow(view, "g1");
    expect(installed.textContent).toContain("reports");
    expect(installed.textContent).toContain("1.2.0");
    expect(installed.textContent).toContain("0123456789abcdef");

    // A server brought by a package sits with it, not with the direct adds.
    expect(directGroup.querySelector('[data-server="s1"]')).toBeTruthy();
    expect(directGroup.querySelector('[data-server="s2"]')).toBeNull();
    expect(packagesGroup.querySelector('[data-server="s2"]')).toBeTruthy();
  });

  it("says whether each server is connected", () => {
    const { view } = renderView();
    const connected = row(view, "s1");
    const missing = row(view, "s2");

    expect(connected.querySelector(".state")?.textContent).toContain("Connected");
    expect(connected.querySelector(".state--done")).toBeTruthy();
    expect(missing.querySelector(".state")?.textContent).toContain("Not connected");
    expect(missing.querySelector(".state--done")).toBeNull();
  });

  it("keeps a member away from the Admin's controls", () => {
    const ownServer: McpServer = {
      server: "s3",
      name: "Mine",
      url: "https://mcp.example.com/mine",
      scope: "user",
      user: "u2",
      by: "u2",
      by_name: "Bo",
      connected: true,
      credential: "user",
    };
    const { view } = renderView({
      account: member,
      servers: [searchServer, ownServer],
      packages: [],
    });

    // No package brought a server, so no package group is shown, and the form
    // offers no choice of a Space-scoped server.
    expect(view.container.querySelector('[data-group="packages"]')).toBeNull();
    expect(view.container.textContent).not.toContain("From packages");
    expect(view.container.querySelector('select[name="server_scope"]')).toBeNull();

    // The member removes their own server, but never another account's.
    expect(button(row(view, "s3"), "Remove")).toBeTruthy();
    expect(row(view, "s1").querySelector(".library__actions")?.textContent).not.toContain(
      "Remove",
    );
  });

  it("shows a member the servers a package brought, without the package list", () => {
    const { view, onConnect, onSignIn, onBot } = renderView({
      account: member,
      servers: [reportServer],
      packages: [],
    });

    // The member cannot read the Admin's package list, so the group names the
    // package neutrally and still shows every server it brought.
    const packages = group(view, "packages");
    expect(packages.textContent).toContain("From packages");
    const installed = packageRow(view, "g1");
    expect(installed.textContent).toContain("Installed package");
    expect(installed.textContent).not.toContain("reports");
    // Enable/Disable is the Admin's, and nobody reads it here.
    expect(installed.querySelectorAll("button")).toHaveLength(0);

    const server = row(view, "s2");
    expect(server).toBeTruthy();
    // The member may sign in to the Space's server for themselves.
    click(button(server, "Sign in"));
    expect(onSignIn).toHaveBeenCalledWith({
      name: "Reports",
      url: "https://mcp.example.com/reports",
      scope: "user",
    });

    // And connect their own credential to it.
    click(button(server, "Connect"));
    type(input(server, 'input[name="connect_token"]'), "member-token");
    submit(server.querySelector("form")!);
    expect(onConnect).toHaveBeenCalledWith("s2", "member-token");

    // And enable it for a Bot they own.
    click(checkbox(server, "Scout"));
    expect(onBot).toHaveBeenCalledWith("b1", "s2", true);
  });

  it("adds a member's server to their own scope", () => {
    const { view, onAdd } = renderView({ account: member, packages: [] });
    type(element<HTMLInputElement>(view, 'input[name="server_name"]'), "Mine");
    type(element<HTMLInputElement>(view, 'input[name="server_url"]'), "https://mcp.example.com/mine");
    submit(element<HTMLFormElement>(view, "form.panel"));

    expect(onAdd).toHaveBeenCalledWith({
      name: "Mine",
      url: "https://mcp.example.com/mine",
      scope: "user",
      token: "",
    });
  });

  it("clears the typed access token and never shows it", async () => {
    const { view, onAdd } = renderView();
    type(element<HTMLInputElement>(view, 'input[name="server_name"]'), "Search");
    type(element<HTMLInputElement>(view, 'input[name="server_url"]'), "https://mcp.example.com");
    const tokenInput = element<HTMLInputElement>(view, 'input[name="server_token"]');
    expect(tokenInput.type).toBe("password");
    type(tokenInput, "add-secret");
    submit(element<HTMLFormElement>(view, "form.panel"));

    await waitFor(() => expect(onAdd).toHaveBeenCalledTimes(1));
    expect(onAdd).toHaveBeenCalledWith({
      name: "Search",
      url: "https://mcp.example.com",
      scope: "space",
      token: "add-secret",
    });
    expect(tokenInput.value).toBe("");
    expect(view.container.innerHTML).not.toContain("add-secret");
  });

  it("clears an inline credential before the answer lands and never shows it", async () => {
    let release!: () => void;
    const onConnect = vi.fn(
      () =>
        new Promise<void>((resolve) => {
          release = resolve;
        }),
    );
    const { view } = renderView({
      servers: [{ ...searchServer, connected: false, credential: "none" }],
      packages: [],
      onConnect,
    });
    const serverRow = row(view, "s1");
    click(button(serverRow, "Connect"));
    const tokenInput = input(serverRow, 'input[name="connect_token"]');
    expect(tokenInput.type).toBe("password");
    type(tokenInput, "connect-secret");
    submit(serverRow.querySelector("form")!);

    await waitFor(() => expect(tokenInput.value).toBe(""));
    expect(onConnect).toHaveBeenCalledWith("s1", "connect-secret");
    expect(view.container.innerHTML).not.toContain("connect-secret");

    release();
    await waitFor(() =>
      expect(serverRow.querySelector('input[name="connect_token"]')).toBeNull(),
    );
  });

  it("gives a server to a Bot and takes it away", () => {
    const { view, onBot } = renderView();
    const serverRow = row(view, "s1");

    const scout = checkbox(serverRow, "Scout");
    const quill = checkbox(serverRow, "Quill");
    expect(scout.checked).toBe(true);
    expect(quill.checked).toBe(false);

    click(quill);
    expect(onBot).toHaveBeenLastCalledWith("b2", "s1", true);

    click(scout);
    expect(onBot).toHaveBeenLastCalledWith("b1", "s1", false);
  });

  it("enables a package the Admin disabled", () => {
    const { view, onPackageEnabled } = renderView({
      packages: [{ ...installedPackage, enabled: false }],
    });
    click(button(packageRow(view, "g1"), "Enable"));
    expect(onPackageEnabled).toHaveBeenCalledWith("g1", true);
  });

  it("offers a browser sign-in for a server without a credential", () => {
    const { view, onSignIn } = renderView({
      servers: [{ ...searchServer, connected: false, credential: "none" }],
      packages: [],
    });
    click(button(row(view, "s1"), "Sign in"));
    // The form's scope decides where the credential belongs; the Space
    // refuses a scope this caller may not name.
    expect(onSignIn).toHaveBeenCalledWith({
      name: "Search",
      url: "https://mcp.example.com/search",
      scope: "space",
    });
  });

  it("offers a sign-in when only the Space's credential exists", () => {
    const { view, onSignIn } = renderView();
    const server = row(view, "s1");
    // The Space's own credential does not stop the reader attaching a personal
    // one, and the token path stays its own button.
    expect(button(server, "Sign in")).toBeTruthy();
    expect(button(server, "Connect")).toBeTruthy();
    click(button(server, "Sign in"));
    expect(onSignIn).toHaveBeenCalledWith({
      name: "Search",
      url: "https://mcp.example.com/search",
      scope: "space",
    });
  });

  it("offers a sign-in to renew a personal credential the Space refuses", () => {
    const { view, onSignIn } = renderView({
      servers: [{ ...searchServer, connected: false, credential: "space-and-user" }],
      packages: [],
    });
    const server = row(view, "s1");
    // The credential is present but does not work, so a sign-in renews it.
    expect(button(server, "Sign in")).toBeTruthy();
    expect(button(server, "Replace credential")).toBeTruthy();
    click(button(server, "Sign in"));
    expect(onSignIn).toHaveBeenCalledWith({
      name: "Search",
      url: "https://mcp.example.com/search",
      scope: "space",
    });
  });

  it("keeps the sign-in away when a personal credential works", () => {
    const { view } = renderView({
      servers: [{ ...searchServer, connected: true, credential: "user" }],
      packages: [],
    });
    const server = row(view, "s1");
    expect([...server.querySelectorAll("button")].map((entry) => entry.textContent?.trim())).not.toContain(
      "Sign in",
    );
    expect(button(server, "Replace credential")).toBeTruthy();
  });

  it("signs in with the scope the form selected", () => {
    const { view, onSignIn } = renderView();
    choose(element<HTMLSelectElement>(view, 'select[name="server_scope"]'), "user");
    click(button(row(view, "s1"), "Sign in"));
    expect(onSignIn).toHaveBeenCalledWith({
      name: "Search",
      url: "https://mcp.example.com/search",
      scope: "user",
    });
  });

  it("clears the form's token and row forms when the scope changes", () => {
    const { view, onScopeChange } = renderView();
    type(element<HTMLInputElement>(view, 'input[name="server_token"]'), "add-secret");
    click(button(row(view, "s1"), "Connect"));
    type(input(row(view, "s1"), 'input[name="connect_token"]'), "row-secret");

    choose(element<HTMLSelectElement>(view, 'select[name="server_scope"]'), "user");

    // The token typed for the old scope is gone, and the row form it was open
    // beside is closed.
    expect(element<HTMLInputElement>(view, 'input[name="server_token"]').value).toBe("");
    expect(row(view, "s1").querySelector('input[name="connect_token"]')).toBeNull();
    expect(view.container.innerHTML).not.toContain("add-secret");
    expect(view.container.innerHTML).not.toContain("row-secret");
    expect(onScopeChange).toHaveBeenCalled();
  });

  it("removes a server the reader may remove", () => {
    const { view, onRemove } = renderView();
    click(button(row(view, "s1"), "Remove"));
    expect(onRemove).toHaveBeenCalledWith("s1");
  });

  it("waits while a command is in flight", () => {
    const { view } = renderView({ busy: true });
    expect(button(row(view, "s1"), "Remove").disabled).toBe(true);
    expect(button(row(view, "s1"), "Connect").disabled).toBe(true);
    expect(element<HTMLButtonElement>(view, 'button[type="submit"]').disabled).toBe(true);
  });

  it("shows the Space's refusal in one alert", () => {
    const { view } = renderView({ error: "The Space could not reach that server." });
    const alerts = view.container.querySelectorAll('[role="alert"]');
    expect(alerts).toHaveLength(1);
    expect(alerts[0]?.textContent).toContain("The Space could not reach that server.");
  });

  it("says the list is loading instead of empty while the first read is in flight", () => {
    const { view } = renderView({ servers: [], packages: [], busy: true });
    expect(view.container.textContent).toContain("Loading…");
    expect(view.container.textContent).not.toContain("No integrations yet");
    expect(view.container.textContent).not.toContain("No packages installed.");
  });

  it("shows the empty list only once the load has finished", () => {
    const { view } = renderView({ servers: [], packages: [], busy: false });
    expect(view.container.textContent).toContain("No integrations yet");
    expect(view.container.textContent).toContain("No packages installed.");
    expect(view.container.textContent).not.toContain("Loading…");
  });

  it("links to a sign-in the browser would not open", () => {
    const { view } = renderView({ signInUrl: "/api/mcp/signin/bind?sign_in=i1" });
    const fallback = element<HTMLElement>(view, "[data-sign-in-fallback]");
    const link = fallback.querySelector("a");
    expect(link?.getAttribute("href")).toBe("/api/mcp/signin/bind?sign_in=i1");
    expect(link?.getAttribute("target")).toBe("_blank");
    expect(fallback.textContent).toContain("The browser blocked the sign-in window.");
  });

  it("waits for a name and an address before adding a server", () => {
    const { view, onAdd } = renderView();
    const form = element<HTMLFormElement>(view, "form.panel");
    submit(form);
    expect(onAdd).not.toHaveBeenCalled();

    type(element<HTMLInputElement>(view, 'input[name="server_name"]'), "   ");
    type(element<HTMLInputElement>(view, 'input[name="server_url"]'), "https://mcp.example.com");
    submit(form);
    expect(onAdd).not.toHaveBeenCalled();

    type(element<HTMLInputElement>(view, 'input[name="server_name"]'), "Search");
    submit(form);
    expect(onAdd).toHaveBeenCalledTimes(1);
    expect(onAdd).toHaveBeenCalledWith({
      name: "Search",
      url: "https://mcp.example.com",
      scope: "space",
      token: "",
    });
  });
});

describe("the fake Space's decoder", () => {
  it("refuses args on a unit command, like the real decoder", async () => {
    await expect(new FakeCommands().command("mcp_list", {})).rejects.toThrow(/carries no args/);
  });

  it("refuses a call that leaves off an args object the real decoder needs", async () => {
    await expect(new FakeCommands().command("skill_list")).rejects.toThrow(/args object/);
  });
});
