import type { MessageKey } from "../i18n";
import type { Approval, Undoable } from "../types";

// The tools whose actions leave nothing to take back, when the Space did not say.
const IRREVERSIBLE = new Set(["shell", "write_file"]);

// The tools whose Always allow covers a directory.
const FILE_TOOLS = new Set(["read_file", "write_file", "list_files"]);

// A summary longer than this, or with more lines, starts folded.
const LONG_SUMMARY = 240;
const LONG_LINES = 3;

// Whether the action can be undone: what the Space says, else the tool's habit.
export function undoableOf(approval: Approval): Undoable {
  return approval.undoable ?? (IRREVERSIBLE.has(approval.tool) ? "no" : "unknown");
}

// What an Always allow would cover, as far as this page can tell without the
// Space's own words: the action's arguments first, else its summary.
export function localScope(approval: Approval): { key: MessageKey; params: Record<string, string> } | null {
  if (approval.tool === "shell") {
    const command = (text(approval.arguments?.command) ?? detailOf(approval)).trim().split(/\s+/)[0];
    return command ? { key: "chat.approval.scope.shell", params: { command } } : null;
  }
  if (FILE_TOOLS.has(approval.tool)) {
    const path = text(approval.arguments?.path) ?? detailOf(approval);
    return path ? { key: "chat.approval.scope.files", params: { path: parentDirectory(path) } } : null;
  }
  return null;
}

function text(value: unknown): string | null {
  return typeof value === "string" && value.trim() !== "" ? value : null;
}

// The summary names the action after its tool, as in "write_file: logs/out.txt".
function detailOf(approval: Approval): string {
  const summary = approval.summary.trim();
  const prefix = `${approval.tool}:`;
  return summary.startsWith(prefix) ? summary.slice(prefix.length).trim() : summary;
}

// The directory a path sits in: "." for a bare name, "/" at the root.
export function parentDirectory(path: string): string {
  const trimmed = path.trim().replace(/\/+$/, "");
  const cut = trimmed.lastIndexOf("/");
  if (cut < 0) return ".";
  if (cut === 0) return "/";
  return trimmed.slice(0, cut);
}

// Whether a summary is long enough to start folded.
export function isLongSummary(summary: string): boolean {
  return summary.length > LONG_SUMMARY || summary.split("\n").length > LONG_LINES;
}

// Everything but tab and newline: a carriage return or an escape sequence in
// an action is something the User should see, not a line break.
const CONTROLS =
  /[\u0000-\u0008\u000b-\u001f\u007f-\u009f\u200b-\u200f\u202a-\u202e\u2060-\u206f\ufeff]/g;

// Shows control and bidi characters as escapes, so a summary cannot mislead
// the eye about what the Bot is about to do. Tabs and newlines stay, since the
// card shows them.
export function escapeControls(text: string): string {
  return text.replace(CONTROLS, (char) => `\\u{${char.codePointAt(0)?.toString(16)}}`);
}
