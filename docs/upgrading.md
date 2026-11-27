# Upgrade a Space

This guide replaces the binary and the built client of a Space that already holds a record, and says what the Space does with that record. It follows [Quick start](quickstart.md), and a Space behind a proxy follows [Serve a Space on a domain](deploying.md) the same way.

A new build starts on the record the last build wrote and migrates it in place. Neha makes no backup before it migrates, so copy the Space's home yourself.

## Back up the Space's home

Read the home the Space uses before you change anything: the `NEHA_HOME` line in its service unit, or the address and `Files are stored in…` lines its startup log prints. Do not set `NEHA_HOME` for a Space you did not start; the running process owns that value.

Set `HOME_DIR` to that path once and use it for the rest of this guide. Keep one shell open for the whole upgrade, so the variable survives.

Stop the Space, then copy the home to a path this run has not used:

```bash
HOME_DIR=/srv/neha/home
BACKUP="$HOME_DIR-backup-$(date +%Y%m%d-%H%M%S).$$"
if [ -e "$BACKUP" ]; then
  echo "already there: $BACKUP" >&2
else
  cp -a "$HOME_DIR" "$BACKUP" && printf 'backup: %s\n' "$BACKUP"
fi
```

`cp -a` writes inside a destination that already exists, so a second upgrade that reuses one path nests the copy inside itself. The timestamp and the shell's process id keep the path new, the check stops the copy when one is already there, and the path prints only after the copy worked. Write it down: a rollback needs the same path.

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

Start the Space again with the same home. Run this in the shell that set `HOME_DIR`, so the Space reads the home you backed up and not another one:

```bash
NEHA_HOME="$HOME_DIR" target/debug/neha serve
```

A service manager already passes that home in its unit, so the unit needs no change; the prefix above is for a start you run yourself. The Space migrates the record's schema in one step and prints its address as before. The Provider, the Bots, the Chats, and every File stay where they were. An open client loads the new build the next time it opens.

A Local Computer is not part of the Space's home. On that machine, replace the daemon binary with the one this release built, then start it again with `neha computer run`. The daemon and the Space must speak the same channel version, and a mismatch closes the connection.

## When the record is newer than the build

The Space refuses to start when the record's schema version exceeds the version this build supports. The Space stops and names both versions:

```text
Error: the Space cannot start

Caused by:
    0: record error: schema error: schema version 7 is newer than 6
```

Two builds that write the same schema version each start on the other's record. Install the newer build, or put your copy back and run the older build again. The two numbers are the record's schema version and this build's, and a build that changes no table and no record vocabulary keeps the version it had.

## What an upgrade does not do

- Neha makes no backup of the Space's home before it migrates the record. The copy you make is the only one.
- Neha restores no home and exports no record, so a rollback is the copy you put back yourself, with the older binary.
- Neha upgrades no Local Computer's daemon. Replace the daemon binary on that machine with this release's, then start it with `neha computer run`.
