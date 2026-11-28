# Neha Desktop

The local-first desktop shell. It asks where the Space lives, starts a bundled
Space only for the local choice, and then loads the existing browser client. The
launcher and the Space share one UI build, one design system, and one sign-in.

The shell uses the local `target/debug/neha` or an explicit `NEHA_BIN`; the
packaged Windows build stages a release `neha.exe` under `resources`.

## Run

From this directory:

```sh
pnpm install
pnpm start
```

The shell needs a built browser client at `ui/dist` with both `index.html` and
`desktop.html`. Build it with the repository UI command, or set `NEHA_UI_DIR`
to an existing `ui/dist`.

Environment:

- `NEHA_BIN` — the `neha` sidecar to run.
- `NEHA_UI_DIR` — the built client directory.
- `NEHA_HOME` — the Space data directory; defaults to `<userData>/space`.
- `NEHA_DESKTOP_HOME` — the app's user-data directory, useful in tests.
- `NEHA_DESKTOP_CAPTURE` — write a screenshot of the loaded page and quit.

## Choose a Space

The first launch shows **Choose a Space**:

- **This computer** starts a private bundled Space and opens its normal
  first-run invitation; the User creates the first Admin and signs in there.
- **Existing Space** takes an `https://` address (loopback `http://` is allowed
  for development), checks it, and loads that Space page: a normal sign-in, or
  the invitation page when the address is an invitation link.

The choice is saved in `desktop-space.json` under the app's user data. Later
launches open that Space directly. A missing or expired session shows the
Space's own sign-in page; the shell never signs in for the User. A Hosted Space
must be served over `https`, normally by a trusted TLS proxy in front of its
loopback listener with `NEHA_PUBLIC_URL` set; see [Quick start](../../docs/quickstart.md#connect-a-client-over-the-network). **Neha →
Change Space…** (`Ctrl/Cmd+Shift+K`) clears the choice, stops the local Space,
and returns to the chooser.

A Hosted Space never starts the bundled Space or the local Computer daemon.

## Local Computer

The **This computer** choice carries an explicit, unchecked Local Computer
checkbox. The launcher text states that its commands run with the Windows
account's rights, in the Bot's folder, with no other OS sandbox. Checking it is
the owner's consent; the shell then starts the bundled Space with
`NEHA_ALLOW_UNSANDBOXED_COMPUTERS=1`.

The daemon starts only after the local Space has a signed-in session. The shell
asks the Space for a one-use ticket with that session, pairs this machine, and
waits for `Connected to` in the daemon's output. Chat remains available if the
daemon cannot start. Without the checkbox, no Local Computer daemon runs.

On Linux the managed folder and its shell fence still apply. On Windows the file
tools stay in the folder but the shell runs with the machine user's rights.
Approvals are the command boundary, not an OS fence. Do not treat Windows as
equivalent to the Linux bubblewrap fence yet.

## Older alpha data

Older desktop alphas created an Admin with a generated password and signed in
automatically. That account has no password its owner knows, so this build
archives `<userData>/space`, `computer`, and `workspace` as
`*.pre-login-<timestamp>`, removes `account.json`, and starts the new flow from
the chooser. The backup stays on disk; the old account is not automatically
signed in.

## Package for Windows

From this directory:

```sh
pnpm package:win
```

The script builds the release sidecar (`x86_64-pc-windows-gnu` on Linux,
`x86_64-pc-windows-msvc` on Windows), builds the UI, stages both under
`resources/`, and runs electron-builder NSIS. The result is
`dist/Neha-Setup-<version>.exe`; it is unsigned.

## Test

```sh
pnpm test
```

The unit tests cover startup-line parsing, Space-address validation, and the
launcher's consent and remote choices. The nightly Windows installer job installs
the app, captures the chooser, opens the local invitation page, and opens a
remote invitation page. Windows runtime tests cover the Local Computer fence. A GUI
session on a developer machine is still the place to inspect the window chrome.
