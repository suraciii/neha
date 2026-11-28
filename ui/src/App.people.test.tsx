import { fireEvent, waitFor } from "@solidjs/testing-library";
import qrcode from "qrcode-generator";
import { flush } from "solid-js";
import { describe, expect, it, vi } from "vitest";

import { ProtocolError } from "./lib/ws";
import { space } from "./test/fakeSpace";
import { button, element, panelText, record, renderApp, useView } from "./test/view";
import type { Invitation } from "./types";

vi.mock("./lib/ws", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./lib/ws")>()),
  Commands: (await import("./test/fakeSpace")).FakeCommands,
}));

type View = ReturnType<typeof renderApp>;

// The invitation the fake Space holds, with the dates its creates use.
function pendingInvitation(id = "inv7"): Invitation {
  return {
    id,
    created_at: new Date(2026, 0, 1, 12).toISOString(),
    expires_at: new Date(2026, 0, 8, 12).toISOString(),
  };
}

// Presses a button by what it says.
function press(view: View, text: string) {
  fireEvent.click(button(view, text));
}

// The row one User's name labels.
function userRow(view: View, name: string): HTMLElement {
  const row = Array.from(view.container.querySelectorAll<HTMLElement>(".row")).find((known) =>
    (known.querySelector(".row__name")?.textContent ?? "").includes(name),
  );
  if (!row) throw new Error(`no ${name} row in the view`);
  return row;
}

// The button the test names inside one row.
function rowButton(row: HTMLElement, text: string): HTMLButtonElement {
  const found = Array.from(row.querySelectorAll<HTMLButtonElement>("button")).find(
    (known) => known.textContent?.trim() === text,
  );
  if (!found) throw new Error(`no ${text} button in the row`);
  return found;
}

// Opens the Settings surface on the section the test names: Providers, Models,
// Computers, People, or the catalog.
async function openSettings(section = "Providers"): Promise<View> {
  const view = renderApp();
  await waitFor(() => expect(element(view, ".rail__item:nth-of-type(4)")).toBeTruthy());
  flush(() => fireEvent.click(element(view, ".rail__item:nth-of-type(4)")));
  // The sidebar lists every section the Settings surface opens.
  await waitFor(() => expect(view.container.querySelectorAll(".conv")).toHaveLength(6));
  flush(() => fireEvent.click(button(view, section)));
  await waitFor(() => expect(element(view, ".page__body")).toBeTruthy());
  return view;
}

describe("Settings People", () => {
  useView();

  it("lists every User with the role each holds and marks the reader", async () => {
    const view = await openSettings("People");

    // The signed-in User is marked, and every role reads in the User's words.
    expect(userRow(view, "Ada").textContent).toContain("(you)");
    expect(userRow(view, "Ada").textContent).toContain("Owner");
    expect(userRow(view, "Grace").textContent).toContain("Admin");
    expect(userRow(view, "Linus").textContent).toContain("Member");
    // Every signed-in User reads the list.
    expect(space.asked).toContain("user_list");
  });

  it("makes an invitation and shows its link and QR code once", async () => {
    const view = await openSettings("People");
    press(view, "Create invitation");

    await waitFor(() => expect(space.invitationSeq).toBe(1));
    // The link is this Space's invitation page carrying the one-time secret.
    await waitFor(() =>
      expect(view.container.querySelector(".invite__link")?.textContent).toBe(
        `${location.origin}/invite#secret1`,
      ),
    );
    // The QR code is an inline SVG drawing the code's own modules.
    const qr = view.container.querySelector('svg[role="img"]');
    expect(qr?.getAttribute("aria-label")).toBe("Invitation QR code");
    expect(qr?.querySelector("path")?.getAttribute("d")).not.toBe("");
    // The answer says the link is one-time and admits a Member.
    const panel = panelText(view, "People");
    expect(panel).toContain("shown only once");
    expect(panel).toContain("one new User as a Member");
    // The invitation now pending is listed with its expiry and the way back.
    expect(panel).toContain("Pending invitation");
    expect(panel).toMatch(/Expires .+ · 12:00/);
    expect(() => button(view, "Revoke")).not.toThrow();
    // One create answers with one panel.
    expect(view.container.querySelectorAll(".invite")).toHaveLength(1);
  });

  it("replaces the one-time panel when another invitation is made", async () => {
    const view = await openSettings("People");
    press(view, "Create invitation");
    await waitFor(() =>
      expect(view.container.querySelector(".invite__link")?.textContent).toBe(
        `${location.origin}/invite#secret1`,
      ),
    );

    press(view, "Create invitation");
    await waitFor(() =>
      expect(view.container.querySelector(".invite__link")?.textContent).toBe(
        `${location.origin}/invite#secret2`,
      ),
    );
    // The first answer left with the second create rather than stacking behind
    // it, so the Space still holds one pending invitation.
    expect(view.container.querySelectorAll(".invite")).toHaveLength(1);
    expect(view.container.querySelectorAll(".invite__qr")).toHaveLength(1);
    expect(panelText(view, "People")).not.toContain("secret1");
    expect(space.invitations).toHaveLength(1);
  });

  it("copies the one-time link", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { writeText },
    });
    try {
      const view = await openSettings("People");
      press(view, "Create invitation");
      await waitFor(() => expect(view.container.querySelector(".invite__link")).toBeTruthy());

      press(view, "Copy link");
      await waitFor(() =>
        expect(writeText).toHaveBeenCalledWith(`${location.origin}/invite#secret1`),
      );
      await waitFor(() => expect(button(view, "Link copied")).toBeTruthy());
    } finally {
      Reflect.deleteProperty(navigator, "clipboard");
    }
  });

  it("shows how to get a link when the answer carries no secret", async () => {
    space.redactInvitation = true;
    const view = await openSettings("People");
    press(view, "Create invitation");

    await waitFor(() =>
      expect(view.container.textContent).toContain(
        "That link was already shown and cannot be shown again.",
      ),
    );
    expect(view.container.querySelector(".invite__link")).toBeNull();
    expect(view.container.querySelector('svg[role="img"]')).toBeNull();
  });

  it("draws the QR code the link makes, with a quiet zone", async () => {
    const view = await openSettings("People");
    press(view, "Create invitation");
    await waitFor(() => expect(view.container.querySelector(".invite__link")).toBeTruthy());

    const link = `${location.origin}/invite#secret1`;
    const expected = qrcode(0, "M");
    expected.addData(link);
    expected.make();
    const count = expected.getModuleCount();
    const quiet = 4;
    const modules: string[] = [];
    for (let row = 0; row < count; row += 1) {
      for (let col = 0; col < count; col += 1) {
        if (expected.isDark(row, col)) modules.push(`M${col + quiet} ${row + quiet}h1v1h-1z`);
      }
    }
    const svg = view.container.querySelector('svg[role="img"]');
    expect(svg?.getAttribute("viewBox")).toBe(`0 0 ${count + quiet * 2} ${count + quiet * 2}`);
    expect(svg?.querySelector("path")?.getAttribute("d")).toBe(modules.join(""));
  });

  it("drops the one-time panel when another Admin replaces the invitation", async () => {
    const view = await openSettings("People");
    press(view, "Create invitation");
    await waitFor(() => expect(view.container.querySelector(".invite__link")).toBeTruthy());

    // Another Admin makes a new link: the pending invitation moves, and the
    // event makes this client read People again.
    space.invitations = [pendingInvitation("inv8")];
    record("neha.invitation.changed", {});
    await waitFor(() => expect(view.container.querySelector(".invite__link")).toBeNull());
    expect(panelText(view, "People")).toContain("Pending invitation");
  });

  it("drops the one-time panel when another Admin revokes the invitation", async () => {
    const view = await openSettings("People");
    press(view, "Create invitation");
    await waitFor(() => expect(view.container.querySelector(".invite__link")).toBeTruthy());

    space.invitations = [];
    record("neha.invitation.changed", {});
    await waitFor(() => expect(view.container.querySelector(".invite__link")).toBeNull());
    await waitFor(() => expect(view.container.textContent).not.toContain("Pending invitation"));
  });

  it("warns that a Local Space's link reaches only nearby devices", async () => {
    const view = await openSettings("People");

    expect(panelText(view, "People")).toContain(
      "A Local Space's link works only from a device that can reach this machine.",
    );
  });

  it("hides the one-time panel when its invitation is revoked", async () => {
    const view = await openSettings("People");
    press(view, "Create invitation");
    await waitFor(() => expect(view.container.querySelector(".invite__link")).toBeTruthy());

    await waitFor(() => expect(button(view, "Revoke")).toBeTruthy());
    press(view, "Revoke");
    await waitFor(() => expect(button(view, "Confirm revoke")).toBeTruthy());
    press(view, "Confirm revoke");
    await waitFor(() => expect(space.invitationRevokes).toHaveLength(1));
    await waitFor(() => expect(view.container.querySelector(".invite__link")).toBeNull());
  });

  it("shows the pending invitation's expiry and revokes it after a second press", async () => {
    space.invitations = [pendingInvitation()];
    const view = await openSettings("People");

    await waitFor(() => expect(panelText(view, "People")).toContain("Pending invitation"));
    expect(panelText(view, "People")).toMatch(/Expires .+ · 12:00/);

    // The first press arms the revoke; the second takes the invitation away.
    press(view, "Revoke");
    await waitFor(() => expect(button(view, "Confirm revoke")).toBeTruthy());
    expect(space.invitationRevokes).toHaveLength(0);

    press(view, "Confirm revoke");
    await waitFor(() => expect(space.invitationRevokes).toEqual(["inv7"]));
    await waitFor(() =>
      expect(element(view, ".page__note").textContent).toBe("Invitation revoked"),
    );
    expect(view.container.textContent).not.toContain("Pending invitation");
  });

  it("changes a Member's role and lists the answer", async () => {
    const view = await openSettings("People");

    press(view, "Make Admin");
    await waitFor(() => expect(space.roleChanges).toEqual([{ user: "u3", role: "admin" }]));
    // The answer's list moves the row to its new role, with the way back.
    await waitFor(() => expect(userRow(view, "Linus").textContent).toContain("Admin"));
    expect(rowButton(userRow(view, "Linus"), "Make Member")).toBeTruthy();
    await waitFor(() => expect(element(view, ".page__note").textContent).toBe("Role saved"));
  });

  it("transfers the Space after a confirmation that names the User", async () => {
    const view = await openSettings("People");

    press(view, "Transfer Space");
    // The first press names the User the Space would move to; nothing happens.
    await waitFor(() =>
      expect(rowButton(userRow(view, "Grace"), "Transfer to Grace?")).toBeTruthy(),
    );
    expect(space.transfers).toHaveLength(0);

    fireEvent.click(rowButton(userRow(view, "Grace"), "Transfer to Grace?"));
    await waitFor(() => expect(space.transfers).toEqual(["u2"]));
    // The answer moves both roles: Grace owns the Space, and Ada is an Admin.
    await waitFor(() => expect(userRow(view, "Grace").textContent).toContain("Owner"));
    expect(userRow(view, "Ada").textContent).toContain("(you)");
    expect(userRow(view, "Ada").textContent).toContain("Admin");
    // Only the Owner changes roles, and Ada is not the Owner any more; the
    // Admins still invite.
    await waitFor(() => expect(() => button(view, "Transfer Space")).toThrow());
    expect(() => button(view, "Make Admin")).toThrow();
    expect(button(view, "Create invitation")).toBeTruthy();
    await waitFor(() => expect(element(view, ".page__note").textContent).toBe("Space transferred"));
  });

  it("lets an Admin invite without offering role changes", async () => {
    space.account.role = "admin";
    space.users = [
      { id: "u1", display_name: "Ada", role: "admin" },
      { id: "u2", display_name: "Grace", role: "owner" },
      { id: "u3", display_name: "Linus", role: "member" },
    ];
    space.invitations = [pendingInvitation()];
    const view = await openSettings("People");

    await waitFor(() => expect(button(view, "Revoke")).toBeTruthy());
    expect(userRow(view, "Ada").textContent).toContain("Admin");
    // The Owner's row carries its role, and the Admin may invite and revoke.
    expect(button(view, "Create invitation")).toBeTruthy();
    expect(button(view, "Revoke")).toBeTruthy();
    expect(() => button(view, "Make Admin")).toThrow();
    expect(() => button(view, "Make Member")).toThrow();
    expect(() => button(view, "Transfer Space")).toThrow();
  });

  it("shows a Member the list alone", async () => {
    space.account.role = "member";
    space.account.is_admin = false;
    space.users = [
      { id: "u1", display_name: "Ada", role: "member" },
      { id: "u2", display_name: "Grace", role: "owner" },
      { id: "u3", display_name: "Linus", role: "member" },
    ];
    const view = await openSettings("People");

    expect(userRow(view, "Ada").textContent).toContain("(you)");
    expect(userRow(view, "Grace").textContent).toContain("Owner");
    expect(view.container.querySelector(".page__error")).toBeNull();
    // A Member neither sees nor asks for the invitation controls.
    expect(() => button(view, "Create invitation")).toThrow();
    expect(() => button(view, "Revoke")).toThrow();
    expect(space.asked).toContain("user_list");
    expect(space.asked).not.toContain("invitation_list");
    expect(space.asked).not.toContain("invitation_create");
  });

  it("reads the Users again when another client moves a role", async () => {
    const view = await openSettings("People");
    expect(userRow(view, "Linus").textContent).toContain("Member");

    // Another client made Linus an Admin: the record says so, and the page
    // reads the list the record now gives.
    space.users = space.users.map((known) =>
      known.id === "u3" ? { ...known, role: "admin" } : known,
    );
    record("neha.user.updated", { user: "u3", role: "admin" });

    await waitFor(() => expect(userRow(view, "Linus").textContent).toContain("Admin"));
  });

  it("follows the reader's own role when another client changes it", async () => {
    space.account.role = "member";
    space.account.is_admin = false;
    space.users = [
      { id: "u1", display_name: "Ada", role: "member" },
      { id: "u2", display_name: "Grace", role: "owner" },
    ];
    const view = await openSettings("People");
    expect(() => button(view, "Create invitation")).toThrow();

    // The Space's Owner made the reader an Admin, which the record reports.
    space.account.role = "admin";
    space.account.is_admin = true;
    space.users = space.users.map((known) =>
      known.id === "u1" ? { ...known, role: "admin" } : known,
    );
    record("neha.user.updated", { user: "u1", role: "admin" });

    // The reader may now invite, and the list says so.
    await waitFor(() => expect(button(view, "Create invitation")).toBeTruthy());
  });

  it("says which role a refused role change needs", async () => {
    const view = await openSettings("People");
    space.refuse.user_set_role = new ProtocolError(
      "forbidden",
      "only the Space Owner changes a role",
    );
    press(view, "Make Admin");

    await waitFor(() =>
      expect(element(view, ".page__error").textContent).toContain(
        "Only the Space Owner can change roles.",
      ),
    );
  });

  it("says which role a refused invitation needs", async () => {
    const view = await openSettings("People");
    space.refuse.invitation_create = new ProtocolError(
      "forbidden",
      "only the Space Owner or an Admin invites a User",
    );
    press(view, "Create invitation");

    await waitFor(() =>
      expect(element(view, ".page__error").textContent).toContain(
        "Only the Space Owner or an Admin can manage invitations.",
      ),
    );
  });

  it("keeps the page's inline error for an invitation that is gone", async () => {
    space.invitations = [pendingInvitation()];
    const view = await openSettings("People");
    space.refuse.invitation_revoke = new ProtocolError("not_found", "no such invitation");

    await waitFor(() => expect(button(view, "Revoke")).toBeTruthy());
    press(view, "Revoke");
    await waitFor(() => expect(button(view, "Confirm revoke")).toBeTruthy());
    press(view, "Confirm revoke");

    await waitFor(() =>
      expect(element(view, ".page__error").textContent).toContain(
        "That record is no longer available.",
      ),
    );
  });
});
