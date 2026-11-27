import type { MessageKey } from "../i18n";
import type { Approval, Undoable } from "../types";

// The tools whose actions leave nothing to take back, when the Space did not say.
const IRREVERSIBLE = new Set(["shell", "write_file"]);

// The tools whose Always allow covers a directory.
const FILE_TOOLS = new Set(["read_file", "write_file", "list_files"]);

// A summary longer than this, or with more lines, starts folded.
const LONG_SUMMARY = 240;
const LONG_LINES = 3;

type Scope = { key: MessageKey; params: Record<string, string> };

// Whether the action can be undone: what the Space says, else the tool's habit.
export function undoableOf(approval: Approval): Undoable {
  return approval.undoable ?? (IRREVERSIBLE.has(approval.tool) ? "no" : "unknown");
}

// What an Always allow would cover, as far as this page can tell without the
// Space's own words: the action's arguments first, else its summary. It follows
// the Space's derivation, so it too gives up on a path that is blank or leaves
// the Workspace. A raw path is trimmed once, at the boundary where the Model
// typed it, exactly as the Space does; a component that survives that trim
// keeps every character it was given, so `logs ` stays `logs `.
export function localScope(approval: Approval): Scope | null {
  if (approval.tool === "shell") {
    const raw =
      typeof approval.arguments?.command === "string"
        ? approval.arguments.command
        : detailOf(approval);
    const word = raw?.trim().split(/\s+/)[0];
    return word ? { key: "chat.approval.scope.shell", params: { command: word } } : null;
  }
  if (FILE_TOOLS.has(approval.tool)) {
    const raw =
      typeof approval.arguments?.path === "string" ? approval.arguments.path : detailOf(approval);
    if (raw === null || (raw.trim() === "" && raw !== "")) return null;
    const derived = raw.trim() === "" ? "." : lexical(raw.trim());
    if (derived.split("/").some((part) => part === ".." || (part !== "" && part.trim() === "")))
      return null;
    const parent = parentOf(derived);
    return parent === "." ? { key: "chat.approval.scope.root", params: {} } : scopeFiles(parent);
  }
  return null;
}

function scopeFiles(path: string): Scope {
  return { key: "chat.approval.scope.files", params: { path } };
}

// The directory a path sits in: "." for a bare name, "/" at the root.
export function parentOf(path: string): string {
  const cut = path.lastIndexOf("/");
  if (cut < 0) return ".";
  if (cut === 0) return "/";
  return path.slice(0, cut);
}

// A path as the machine reads it: "." steps and doubled separators removed.
function lexical(path: string): string {
  const parts = path.split("/").filter((part) => part !== "" && part !== ".");
  const joined = parts.join("/");
  if (joined === "") return path.startsWith("/") ? "/" : ".";
  return (path.startsWith("/") ? "/" : "") + joined;
}

// The summary names the action after its tool, as in "write_file: logs/out.txt".
// Any other summary is prose for the User, not a path or a command.
function detailOf(approval: Approval): string | null {
  const summary = approval.summary.trim();
  const prefix = `${approval.tool}:`;
  const detail = summary.startsWith(prefix) ? summary.slice(prefix.length).trim() : "";
  return detail || null;
}

// Whether a summary is long enough to start folded.
export function isLongSummary(summary: string): boolean {
  return summary.length > LONG_SUMMARY || summary.split("\n").length > LONG_LINES;
}

// Everything but tab and newline: a carriage return or an escape sequence in
// an action is something the User should see, not a line break.
const CONTROLS =
  // biome-ignore lint/suspicious/noControlCharactersInRegex: finding them is this expression's purpose.
  /[\u0000-\u0008\u000b-\u001f\u007f-\u009f\u200b-\u200f\u202a-\u202e\u2060-\u206f\ufeff]/g;

// Shows control and bidi characters as escapes, so a summary cannot mislead
// the eye about what the Bot is about to do. Tabs and newlines stay, since the
// card shows them.
export function escapeControls(text: string): string {
  return text.replace(CONTROLS, (char) => `\\u{${char.codePointAt(0)?.toString(16)}}`);
}
