// Pure startup helpers for the desktop shell. The Electron entry point imports
// these; unit tests can require this file without starting Electron.
const crypto = require("node:crypto");

/**
 * Reads one line of the Space's startup output.
 *
 * The CLI prints the loopback address first and, on a fresh Space, an
 * invitation line and a browser URL. Keeping this parser separate makes the
 * line protocol testable without spawning a Space.
 *
 * @param {string} line
 * @returns {{ listening?: string, invite?: string, browser?: string } | null}
 */
function parseStartupLine(line) {
  const listening = line.match(/Neha is listening on (http:\/\/\S+)/);
  if (listening) return { listening: listening[1] };

  const invite = line.match(/Invite: (http:\/\/\S+)/);
  if (invite) return { invite: invite[1] };

  const browser = line.match(/Open in a browser: (http:\/\/\S+)/);
  if (browser) return { browser: browser[1] };

  return null;
}

/**
 * Extracts the first name/value pair from a Set-Cookie header.
 *
 * The Space emits its own session cookie name, so the desktop shell must use
 * the name from the response instead of assuming one.
 *
 * @param {string | null | undefined} value
 * @returns {{ name: string, value: string } | null}
 */
function cookiePair(value) {
  if (!value) return null;
  const first = value.split(";", 1)[0]?.trim();
  if (!first) return null;
  const separator = first.indexOf("=");
  if (separator <= 0) return null;
  const name = first.slice(0, separator).trim();
  const cookieValue = first.slice(separator + 1).trim();
  if (!name || !cookieValue) return null;

  return { name, value: cookieValue };
}

/**
 * Returns a display name the built-in Space can use without asking the user.
 *
 * The OS account name is a local default, not an identity claim. The Space
 * caps display names at 64 characters; an empty account name falls back to
 * "Admin".
 *
 * @param {string | undefined | null} username
 * @returns {string}
 */
function defaultDisplayName(username) {
  const trimmed = (username ?? "").trim();
  return (trimmed === "" ? "Admin" : trimmed).slice(0, 64);
}

/** Generates the internal password for the first local Admin. */
function generatePassword() {
  return crypto.randomBytes(32).toString("base64url");
}

module.exports = {
  cookiePair,
  defaultDisplayName,
  generatePassword,
  parseStartupLine,
};
