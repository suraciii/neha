# Neha Desktop

The local-first desktop shell. It starts the bundled Space, creates the first
local Admin without showing the invitation form, and loads the existing browser
client.

This is the first slice of the Windows desktop work. It currently uses the local
`target/debug/neha` or an explicit `NEHA_BIN`; packaging the sidecar and Windows
Local Computer are follow-up work.

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
the password with Electron `safeStorage` (Windows DPAPI), and loads the client.

On a Space that already has an account, the shell discovers the browser origin,
signs in with the stored account, and loads the client. The invitation form is
not shown in either path.

## Test

```sh
pnpm test
```

The tests cover the startup-line parser, cookie parsing, display-name fallback,
and generated passwords. The Electron integration is exercised manually with a
real Space until the packaging and native CI slices exist.
