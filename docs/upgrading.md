# Upgrade a Space

This guide replaces the binary and the built client of a Space that already holds a record, and says what the Space does with that record. It follows [Quick start](quickstart.md), and a Space behind a proxy follows [Serve a Space on a domain](deploying.md) the same way.

A new build starts on the record the last build wrote and migrates it in place. Neha makes no backup before it migrates, so copy the Space's home yourself.

## Back up the Space's home

Stop the Space, then copy the home directory:

```bash
export NEHA_HOME="$HOME/.local/share/neha/demo"
cp -a "$NEHA_HOME" "$NEHA_HOME.bak"
```

Copy the home while the Space is stopped, so the record and the objects in the File store agree. With an S3-compatible File store, copy the objects under the Space's prefix too; [Storage](storage.md#backup) gives the table and the restore steps.

Keep the copy until the new build has started once and the Space answers.

## Replace the binary and the client

Build the workspace and the browser client, as [Building and verification](building.md) shows:

```bash
cargo build --locked --workspace
pnpm --dir ui install --frozen-lockfile
pnpm --dir ui build
```

Put the new `neha` where the Space's command or its service unit names it, and put the built client where `--ui` or `NEHA_UI_DIR` points. A service manager starts the new binary on its next start.

## Start the Space again

Start the Space with the same `NEHA_HOME`:

```bash
target/debug/neha serve
```

The Space migrates the record's schema in one step and prints its address as before. The Provider, the Bots, the Chats, and every File stay where they were. An open client loads the new build the next time it opens.

A Local Computer is not part of the Space's home. Restart its daemon with the `neha computer run` of the same release; a daemon from another release is refused the same way.

## When the record is newer than the build

A home that a newer build wrote does not start on an older one. The Space stops and names the version it found and the version this build writes:

```text
Error: the Space cannot start

Caused by:
    0: record error: schema error: schema version 7 is newer than 6
```

Install the newer build, or put your copy back and run the older build again. The two numbers are the record's version and this build's version, so they change with each release.

## What an upgrade does not do

- Neha makes no backup of the Space's home before it migrates the record. The copy you make is the only one.
- Neha restores a home and exports no record, so a rollback is your copy put back with the older binary.
- Neha upgrades a Local Computer's daemon with the Space. Restart it with the `neha computer run` of the same release.
