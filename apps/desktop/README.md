# Neha Desktop

The local-first desktop shell. It starts the bundled Space, creates the first
local Admin without showing the invitation form, starts the Local Computer, and
loads the existing browser client. No separate server is installed or operated.

The shell currently uses the local `target/debug/neha` or an explicit `NEHA_BIN`;
the packaged Windows build stages a release `neha.exe` under `resources`.

## Run

From this directory:

```sh
pnpm install
pnpm start
```

The shell needs a built browser client at `ui/dist`. Build it with the repository
UI command, or set `NEHA_UI_DIR` to an existing `ui/dist`.

Environment:

- `NEHA_BIN` — the `neha` sidecar to run.
- `NEHA_UI_DIR` — the built client directory.
- `NEHA_HOME` — the Space data directory; defaults to `<userData>/space`.
- `NEHA_DESKTOP_HOME` — the app's user-data directory, useful in tests.
- `NEHA_DESKTOP_CAPTURE` — write a screenshot of the loaded client and quit.

## First run and later runs

On a fresh Space the shell reads the one-time invitation, creates the first
Admin with the local OS account name and a generated internal password, stores
the password with Electron `safeStorage` (Windows DPAPI), restarts the Space so
it writes its one-use Local Computer ticket, pairs this machine, starts the
Local Computer daemon, and loads the client.

On a Space that already has an account, the shell updates the saved Computer
pairing to the new loopback port, starts the daemon, signs in, and loads the
client. The invitation form is not shown in either path.

## Windows Local Computer

The Windows runtime is a managed Bot folder: its file tools stay inside the
folder, but its shell runs with the machine user's rights and has **no OS
sandbox** in this slice. Approvals are the command boundary. Do not treat it as
equivalent to the Linux bubblewrap fence yet.

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

The tests cover the startup-line parser, cookie parsing, display-name fallback,
and generated passwords. Native Windows CI builds and installs the app and
launches it against a real first-run Space; a GUI session on a developer machine
is still needed to inspect the window.
