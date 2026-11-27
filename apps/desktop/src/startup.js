// Pure startup helpers for the desktop shell. The Electron entry point imports
// these; unit tests can require this file without starting Electron.

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

module.exports = { parseStartupLine };
