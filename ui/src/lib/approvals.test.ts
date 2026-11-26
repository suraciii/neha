import { describe, expect, it } from "vitest";

import { escapeControls, isLongSummary, localScope, parentDirectory, undoableOf } from "./approvals";
import type { Approval } from "../types";

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

describe("approvals", () => {
  it("takes the Space's word on undoing, else the tool's habit", () => {
    expect(undoableOf({ ...shell, undoable: "unknown" })).toBe("unknown");
    expect(undoableOf({ ...shell, tool: "read_file", undoable: "no" })).toBe("no");
    expect(undoableOf(shell)).toBe("no");
    expect(undoableOf({ ...shell, tool: "write_file" })).toBe("no");
    expect(undoableOf({ ...shell, tool: "read_file" })).toBe("unknown");
    expect(undoableOf({ ...shell, tool: "list_files" })).toBe("unknown");
  });

  it("derives the scope an Always allow would cover", () => {
    // The arguments come first; the summary only speaks when they are absent.
    expect(
      localScope({ ...shell, arguments: { command: "git status" }, summary: "shell: rm -rf /" }),
    ).toEqual({ key: "chat.approval.scope.shell", params: { command: "git" } });
    expect(
      localScope({
        ...shell,
        tool: "write_file",
        arguments: { path: "logs/out.txt" },
        summary: "write_file: /etc/passwd",
      }),
    ).toEqual({ key: "chat.approval.scope.files", params: { path: "logs" } });
    // A summary names the action after its tool.
    expect(localScope({ ...shell, tool: "write_file", summary: "write_file: logs/out.txt" })).toEqual({
      key: "chat.approval.scope.files",
      params: { path: "logs" },
    });
    expect(localScope({ ...shell, summary: "shell: printf hi" })).toEqual({
      key: "chat.approval.scope.shell",
      params: { command: "printf" },
    });
    expect(localScope(shell)).toEqual({ key: "chat.approval.scope.shell", params: { command: "git" } });
    // Nothing to derive from says nothing.
    for (const blank of [
      { ...shell, tool: "write_file", summary: "write_file: " },
      { ...shell, tool: "write_file", arguments: { path: 42 }, summary: "write_file:  " },
      { ...shell, tool: "take_file" },
      { ...shell, summary: "   " },
    ]) {
      expect(localScope(blank)).toBeNull();
    }
  });

  it("finds the directory a path sits in", () => {
    expect(parentDirectory("notes.txt")).toBe(".");
    expect(parentDirectory("logs/deep/a.txt")).toBe("logs/deep");
    expect(parentDirectory("/etc/passwd")).toBe("/etc");
    expect(parentDirectory("/passwd")).toBe("/");
    expect(parentDirectory("logs/")).toBe(".");
    expect(parentDirectory(" spaced/a.txt ")).toBe("spaced");
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
