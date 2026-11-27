# Upgrade a Space

This guide replaces the binary and the built client of a Space that already holds a record, and says what the Space does with that record. It follows [Quick start](quickstart.md), and a Space behind a proxy follows [Serve a Space on a domain](deploying.md) the same way.

A new build starts on the record the last build wrote and migrates it in place. Neha makes no backup before it migrates, so copy the Space's home yourself.

## Back up the Space's home

Read the home the Space uses before you change anything: the `NEHA_HOME` line in its service unit, or the address and `Files are stored in…` lines its startup log prints. Do not set `NEHA_HOME` for a Space you did not start; the running process owns that value.

Stop the Space, then copy that home to a path this run has not used:

```bash
HOME_DIR=/srv/neha/home
BACKUP="$HOME_DIR-backup-$(date +%Y%m%d-%H%M%S).$$"
cp -a "$HOME_DIR" "$BACKUP"
printf 'backup: %s\n' "$BACKUP"
```

`cp -a` writes inside a destination that already exists, so a second upgrade that reuses one path nests the copy inside itself. The timestamp and the shell's process id keep each run's path new. Write the path down: a rollback needs the same one.

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

Start the Space with the same home its unit or its startup log named:

```bash
target/debug/neha serve
```

A service manager passes that home itself. The Space migrates the record's schema in one step and prints its address as before. The Provider, the Bots, the Chats, and every File stay where they were. An open client loads the new build the next time it opens.

A Local Computer is not part of the Space's home. On that machine, replace the daemon binary with the one this release built, then start it again with `neha computer run`. The daemon and the Space must speak the same channel version, and a mismatch closes the connection.

## When the record is newer than the build

A home that a newer build wrote does not start on an older one. The Space stops and names the schema version it found and the version this build writes:

```text
Error: the Space cannot start

Caused by:
    0: record error: schema error: schema version 7 is newer than 6
```

Install the newer build, or put your copy back and run the older build again. The two numbers are the record's schema version and this build's, and a build that changes no table and no record vocabulary keeps the version it had.

## What an upgrade does not do

- Neha makes no backup of the Space's home before it migrates the record. The copy you make is the only one.
- Neha restores no home and exports no record, so a rollback is the copy you put back yourself, with the older binary.
- Neha upgrades no Local Computer's daemon. Replace the daemon binary on that machine with this release's, then start it with `neha computer run`.
