import { describe, expect, it } from "vitest";
import type { Approval } from "../types";
import { escapeControls, isLongSummary, localScope, parentOf, undoableOf } from "./approvals";

// An approval whose summary is prose for the User, not a tool-prefixed action.
const shell: Approval = {
  approval: "a1",
  run: "r1",
  bot: "b1",
  chat: "h1",
  action: "act1",
  step: 1,
  tool: "shell",
  summary: 'git status  "two words"',
  reason: "first_use_local",
  rule: null,
};

const scopeShell = (command: string) => ({ key: "chat.approval.scope.shell", params: { command } });
const scopeFiles = (path: string) => ({ key: "chat.approval.scope.files", params: { path } });
const scopeRoot = { key: "chat.approval.scope.root", params: {} };

describe("approvals", () => {
  it("takes the Space's word on undoing, else the tool's habit", () => {
    expect(undoableOf({ ...shell, undoable: "unknown" })).toBe("unknown");
    expect(undoableOf({ ...shell, tool: "read_file", undoable: "no" })).toBe("no");
    expect(undoableOf(shell)).toBe("no");
    expect(undoableOf({ ...shell, tool: "write_file" })).toBe("no");
    expect(undoableOf({ ...shell, tool: "read_file" })).toBe("unknown");
    expect(undoableOf({ ...shell, tool: "list_files" })).toBe("unknown");
  });

  it("derives the scope an Always allow would cover, as the Space does", () => {
    const file = (path: unknown, summary = "write_file: logs/out.txt") => ({
      ...shell,
      tool: "write_file",
      arguments: { path },
      summary,
    });

    // The arguments come first; the summary only speaks when they are absent.
    expect(
      localScope({ ...shell, arguments: { command: "git status" }, summary: "shell: rm -rf /" }),
    ).toEqual(scopeShell("git"));
    expect(localScope(file("cache/tmp.txt"))).toEqual(scopeFiles("cache"));
    expect(
      localScope({ ...shell, tool: "write_file", arguments: { path: "out/report.pdf" } }),
    ).toEqual(scopeFiles("out"));
    // A summary names the action after its tool; other prose names nothing.
    expect(
      localScope({ ...shell, tool: "write_file", summary: "write_file: logs/out.txt" }),
    ).toEqual(scopeFiles("logs"));
    expect(localScope({ ...shell, summary: "shell: printf hi" })).toEqual(scopeShell("printf"));
    expect(
      localScope({ ...shell, tool: "read_file", summary: "read_file on this Computer" }),
    ).toBeNull();
    expect(localScope(shell)).toBeNull();
    // A bare name sits in the Workspace root, which has its own words.
    expect(localScope(file("notes.txt"))).toEqual(scopeRoot);
    expect(localScope(file(""))).toEqual(scopeRoot);
    expect(localScope(file("/etc/passwd"))).toEqual(scopeFiles("/etc"));
    // The machine reads the path once, so "." steps and doubled separators go.
    expect(localScope(file("logs/./a//b.txt"))).toEqual(scopeFiles("logs/a"));
    // A directory that differs by a trailing space stays its own.
    expect(localScope(file("logs /a"))).toEqual(scopeFiles("logs "));
    // The Space derives nothing from a path that is blank or leaves the Workspace.
    for (const path of ["../x", "logs/../x", "/../x", "   "]) {
      expect(localScope(file(path))).toBeNull();
    }
    expect(localScope(file(42, "write_file:  "))).toBeNull();
    expect(localScope({ ...shell, tool: "unknown_tool" })).toBeNull();
    expect(localScope({ ...shell, arguments: { command: "   " }, summary: "shell: " })).toBeNull();
  });

  it("finds the directory a path sits in", () => {
    expect(parentOf("notes.txt")).toBe(".");
    expect(parentOf("logs/deep/a.txt")).toBe("logs/deep");
    expect(parentOf("/etc/passwd")).toBe("/etc");
    expect(parentOf("/passwd")).toBe("/");
  });

  it("folds only a long summary", () => {
    expect(isLongSummary("printf hi")).toBe(false);
    expect(isLongSummary("a".repeat(241))).toBe(true);
    expect(isLongSummary("one\ntwo\nthree")).toBe(false);
    expect(isLongSummary("one\ntwo\nthree\nfour")).toBe(true);
  });

  it("shows control characters as escapes and keeps whitespace", () => {
    expect(escapeControls("printf \u001b[31mred\u001b[0m\rnext")).toBe(
      "printf \\u{1b}[31mred\\u{1b}[0m\\u{d}next",
    );
    expect(escapeControls("a\u202eb.txt")).toBe("a\\u{202e}b.txt");
    expect(escapeControls("two\tlines\nand more")).toBe("two\tlines\nand more");
  });
});
