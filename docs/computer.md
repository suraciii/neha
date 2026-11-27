# Add a Local Computer

This guide turns the machine that runs a Space into a Local Computer: a Neha process that connects out to the Space and runs each Bot's actions in that Bot's Workspace on the machine. It follows [Quick start](quickstart.md), so the Space, its Admin, a Provider, and a Chat already exist.

This milestone pairs the machine that runs the Space, because the Space listens only on loopback. [What this milestone does not do](#what-this-milestone-does-not-do) names that and the other limits.

A new Bot starts on a Hosted Computer unless `--computer` names a Local Computer. [The Hosted Computer](#the-hosted-computer) covers that kind.

## What you need

- A Linux machine with the Neha CLI. [Building and verification](building.md) covers the build; the examples use `target/debug/neha`.
- `bubblewrap` on that machine. A Local Computer runs each shell command in a sandbox it starts, unless the Bot has full access, and the machine that runs the Space needs it for a Hosted Computer too; the file tools check their own paths. [Building and verification](building.md#a-computer-sandbox) covers the install.
- A Space running on the same machine, at an address the Local Computer can reach; in this milestone that is the host that runs the Space.
- A CLI signed in to the Space for commands such as `computer list` and `bot create`, and for the HTTP fallback that asks for a ticket; pairing from a startup ticket file needs no sign-in. [Quick start](quickstart.md#join-as-the-admin-from-the-cli) covers `neha join`.

## Pair the machine

Start the Space with `neha serve` and keep it running. A Space that had an Admin when it started also prints the command that pairs this machine:

```text
Pair this PC: neha computer pair
```

Run it in another terminal:

```bash
target/debug/neha computer pair
```

No pairing code is entered. The command reads the startup ticket the Space wrote under this machine's Neha configuration directory, or, when there is none, asks the Space for one with the signed-in CLI. It then asks for confirmation:

```text
Pair this Computer with the Space at http://127.0.0.1:7337? [y/N]
```

Answer `y`, and it prints the new Computer:

```text
Paired Computer <computer-id> with http://127.0.0.1:7337.
```

The command writes the machine's configuration and stops. Start the daemon, which connects out to the Space:

```bash
target/debug/neha computer run
```

Keep that terminal open. The Space shows the Computer as online while the daemon runs, and the run prints the Workspace root it serves.

The ticket admits one pairing for one Space start and expires after 10 minutes. A Space that started with an Admin writes its startup ticket as one file per Space under the machine's Neha configuration directory, with mode 0600, which granted mode hides from Bots.

Each Space's ticket sits at `$XDG_CONFIG_HOME/neha/spaces/<space-key>/local-computer.ticket`, or `~/.config/neha/spaces/<space-key>/local-computer.ticket` when `XDG_CONFIG_HOME` is not set; on Windows it is under `%LOCALAPPDATA%\Neha\spaces`. `computer pair` scans `spaces/` and pairs the Space whose URL its ticket names; `--ticket-file` names another file. When no ticket file exists, a signed-in CLI asks the Space that saved its session for one, so only that fallback needs a signed-in CLI.

The Computer belongs to the User the ticket names: the Admin for a startup ticket, or the signed-in User who asked for one.

Run `neha computer pair` again if the ticket expired or was used, and restart the Space first if it wrote the old one. Pass `--yes` to skip the confirmation, `--space <url>` to name the Space address, and `--ticket-file <path>` to name another ticket file. `--space` accepts only a loopback address in this milestone, and the fallback calls only the Space that saved the CLI session.

The machine keeps its configuration in `~/.config/neha/computer.json` with mode 0600. `$XDG_CONFIG_HOME/neha` replaces that directory when `XDG_CONFIG_HOME` is set, and `NEHA_COMPUTER_HOME` replaces both. The Bots' Workspaces go in `~/.neha/workspaces` by default; pass `--workspace <directory>` to `computer pair` to put them under another root, and a root inside the configuration directory is refused.

An older Neha kept its pairing in `~/.neha/computer.json` and its Workspaces in `~/Neha`. While that file exists, `computer pair` and `computer run` refuse; the old file and the old Workspaces are not read or moved. Delete the old file, or move it into storage that the machine's Bots cannot read, then pair again; the old Workspaces stay where they are. Storage outside the machine is the safe place for a copy.

The old file may still hold a valid credential. Renaming it in place does not hide it from the machine's Bots, so keep any copy private.

Re-pairing adds a new Computer; the Bots on the old one stay bound to it, and their Workspaces and queued work stay there. Move a Bot to the new Computer with `neha bot move`; [Moving a Bot](#moving-a-bot) covers the details.

## Create a Bot on the machine

List the Computers you own:

```bash
target/debug/neha computer list
```

Each line gives one Computer's id, kind, online state, and workspace. The kind is `local` for a paired machine and `hosted` for one the Space runs. Put your Local Computer's id from the list above in `COMPUTER_ID`, and create a Bot on it:

```bash
COMPUTER_ID="<the id of your Local Computer>"
target/debug/neha bot create Runner --computer "$COMPUTER_ID"
RUNNER_ID=$(target/debug/neha bot list | awk '$2 == "Runner" {print $1; exit}')
```

`--computer` takes `local`, `hosted`, or a Computer id:

| Value | What happens |
| --- | --- |
| `local` | The owner's Local Computer; with several, the earliest one recorded, which may be an old or offline machine |
| `hosted` | A new Hosted Computer, one per Bot |
| A Computer id | The Local Computer with that id |

A Local Computer's workspace column is empty when the Space recorded no path for it; a Local Computer paired with `computer pair` records none, and `neha bot show` then omits its Workspace line. The root that `neha computer run` prints as `Workspace root:` is the one its actions use.

Without `--computer`, a new Bot starts on a Hosted Computer; `--computer local` puts it on your paired Local Computer. A Hosted Computer belongs to its Bot, so naming its id is refused; use `--computer hosted` for a new one.

Ask the Bot to work and check where the work ran:

```bash
target/debug/neha chat send --bot "$RUNNER_ID" "pwd; echo hi > a.txt"
```

The first action on this Local Computer asks for approval in the Chat: the Bot waits until you Allow or Deny, and the first use has no Always allow. Each Local Computer asks once for each Bot and User, so a Bot moved to another Local Computer asks again there. [Approvals](approvals.md) covers the card and the commands.

## The Hosted Computer

A new Bot starts on a Hosted Computer unless `--computer` names a Local Computer. A Hosted Computer needs no machine of yours: it runs on the Space's machine.

Its shell runs in a `bubblewrap` sandbox built from an empty root, with the machine's system directories mounted read-only.

The sandbox mounts the Workspace at `/workspace`, which is its working directory and `HOME`. That folder is the only place where files persist; the sandbox has its own process list, `/dev`, `/proc`, a fresh `/tmp`, and `/dev/shm`.

The sandbox ends with the Space process. It cannot see the Space's record, its secrets, its File store, or another Computer's Workspace. The sandbox's network is the machine's, so it reaches the Space's loopback.

The folder holding the `neha` binary is mounted read-only at `/opt/neha-cli` and added to `PATH`, so `neha file get` and `neha chat send --file` work in the shell. [Send and receive Files](files.md#let-the-bot-take-and-deliver) covers them.

The Workspace lives in the Space's home:

```text
$NEHA_HOME/computers/<computer-id>/workspace
```

A Space restart keeps it, and no command is needed. It is not an independent image: the system directories are the machine's and read-only, so a system package cannot be installed. A Hosted Computer has no screen, browser, Vault credential, or Take control yet.

`neha computer reset <computer-id>` clears the Workspace: every file and folder inside it is deleted, and the Workspace folder stays. Only the Computer's owner can reset it.

A run that is queued, running, or waiting refuses the reset with a conflict until it ends. A second reset of an empty Workspace succeeds, and a reset restores no file.

The Space machine needs `bubblewrap` for a Hosted Computer. Where it cannot start the sandbox, `bot create` fails and names `--computer local` as the alternative; it does not fall back. The Space also refuses a Hosted Computer when `NEHA_HOME` would be inside a read-only system directory or the CLI directory, or when an existing Workspace holds a link.

## The Bot's Workspace and the write fence

On a Local Computer, the Workspace root is `~/.neha/workspaces` by default; `--workspace` on `computer pair` chooses another root. The root holds one folder for each Bot, named by the Bot's id:

```text
~/.neha/workspaces/<bot-id>/
```

The folder is created when the Bot first runs an action. Every action of the Bot starts in its Workspace, or in the directory `--cwd` names for that run: a relative path names a file inside it, and `pwd` prints it. The `Workspace root:` line from `neha computer run` names the root the daemon actually serves.

The file tools check every write path. In granted mode a run writes its Workspace, the granted folders, and the `--cwd` directory it named, and a file written in that directory stays on the machine; a `..` climb or a symlink out is refused, and no file is created. [File access and a run's directory](#file-access-and-a-runs-directory) covers a write outside that range and what full access changes.

The file tools read with the rights of the machine's user. A file read follows a symlink as the machine user's own tools do, and reaches any file that user can read, including files outside the Workspace and files of another Bot on the machine. A directory listing does not follow a symlink that is the path itself, and a FIFO or a device is not read.

In granted mode the Computer's own configuration directory is the exception. A path that resolves into it is refused, even through a symlink; the daemon hides it from the file tools and the shell, so a Bot cannot read the saved pairing or its token. The directory is `~/.config/neha` by default. A Bot with full access reads it as the machine's user does.

The same split reaches Files: in the shell, `neha file get` writes a File of the Chat into the Workspace, and `neha chat send --file` reads a Workspace file and sends it to the Chat. Both run as shell commands and go through the approval gate like other shell commands. [Send and receive Files](files.md#let-the-bot-take-and-deliver) covers them.

In granted mode on Linux a shell command runs in an operating-system sandbox: the machine is read-only, the Bot's Workspace, its granted folders, and the run's `--cwd` directory are writable, and an empty tmpfs covers the Computer's configuration directory. A command also gets a fresh `/tmp` inside the sandbox; a write outside the writable roots, such as one in that `/tmp`, is gone when the command ends and never reaches the machine's own `/tmp`.

On Windows the sandbox is not available. This is an explicit opt-in mode, not a silent fallback: the Space operator sets `NEHA_ALLOW_UNSANDBOXED_COMPUTERS=1`, and the desktop asks the owner to accept an in-launcher warning before it starts the Local Computer. The file tools stay inside the Bot's folder, but a shell command runs with the machine user's rights and can reach the rest of the machine. Grants, full access, and `--cwd` are refused on Windows rather than silently narrowed. Only use trusted Bots; approvals are the command boundary, not an OS fence.

In granted mode on Linux the daemon checks a Bot's folder before it serves it. A hard link, a symlink, or a special file in the folder makes the daemon refuse that folder until it is cleaned; keep links and special files out of the Workspace. A Bot with full access is served without that check.

On Linux, a machine whose `bubblewrap` cannot start a sandbox does not serve as a Computer.

## Moving a Bot

Move a Bot from one of your Local Computers to another. The target is the other machine, so its id is not the `COMPUTER_ID` you created the Bot with:

```bash
BOT_ID=$(target/debug/neha bot list | awk '$2 == "Runner" {print $1; exit}')
COMPUTER_ID="<the id of the other Local Computer>"
target/debug/neha bot move "$BOT_ID" --computer "$COMPUTER_ID"
```

The command prints the Computer the Bot now works on. The Bot's memory, its Chats, and the Files it delivered stay with it.

The old Computer keeps the Bot's folder, so work the Bot did not deliver stays there. Moving does not copy files: the Bot's first action on the new Computer creates its folder when it does not exist; when it does exist, for example when a second Computer on the same machine reuses one `--workspace`, the Bot uses the files already there. To show an empty folder on one machine, give the new Computer its own `--workspace`.

A run already under way finishes on the old Computer. A run that starts after the move uses the new Computer, including a request that was still waiting in the queue.

The old Computer may be offline when you move the Bot. If the new Computer is offline too, the move is still recorded, and waiting work runs when it comes back. A Bot on a Hosted Computer cannot move: create a new Bot on the target Computer instead.

## While the machine is offline

A Computer is online while its daemon holds the channel to the Space. If the daemon stops, the Computer shows as offline in `neha computer list`, and a Message sent to one of its Bots waits in the record.

The Space and the daemon must come from the same release. The channel speaks version 2, and the Space refuses a daemon whose channel version does not match. A daemon left on an older release connects and is refused; restart it with the `neha computer run` of this release.

`neha chat send` waits up to 180 seconds for the run to end. If the run is still queued, the command stops with `the run did not end within 180s`, and the Message and its run stay in the record.

When the daemon starts again with `neha computer run`, it reconnects and queued runs start. Read the Chat later for the reply; do not send the same Message again. `neha chat list` names the Chats, and `neha chat read <chat-id>` shows their Messages.

If the connection drops while an action runs, the Space records that action as unknown and fails the run. The Bot tells the Chat that it lost its Computer. The run does not resume.

If the Space itself stops while a run works, the next start continues the run from its record. Completed steps replay into the Model's context; an action that started and never returned is checked, not run again: a `write_file` counts as done only when the file holds exactly the requested content, and any other unfinished action makes the run fail with its result unknown. A step whose Model reply never arrived is called again from its start; that call may be billed twice even though the Space did not keep its usage. A recovered continuation can still fail if its Provider is unavailable. The Bot answers once, and no recorded action runs twice.

A run recorded before the Space recorded the access it starts with continues with granted access only: no full access and no grants. A run whose record names a working directory from before the Space marked one writable does not continue: the Bot asks you to send the message again, and that directory is not made a write root.

The machine ends the command it was running when the channel drops, so no action keeps running unattended.

## Troubleshooting

| Symptom | What to do |
| --- | --- |
| `pair` asks for a Space URL | Pass `--space <url>`, or sign in from the CLI so it can ask the Space for a ticket. |
| `pair` stops before printing a Computer | Read the error it prints. A ticket that expired or was used, a workspace directory it cannot create, and a Space it cannot reach are the common causes. |
| `pair` or `computer run` reports an older pairing | The old `~/.neha/computer.json` blocks both commands while it exists. Delete it, or move it into storage the machine's Bots cannot read, then run `neha computer pair`; the new pairing goes in `~/.config/neha/computer.json`. Any copy may still hold a valid credential, so keep it private. |
| A `computer list` workspace is empty | A Local Computer's workspace is empty when the Space recorded no path for it; a Local Computer paired with `computer pair` records none. `neha computer run` prints the real root. |
| An older record uses `real` or `managed` | An old `real` Computer reads as Local and an old `managed` one as Hosted. No action is needed. |
| `pair` or `computer run` reports the workspace is inside the configuration directory | The configuration directory is `~/.config/neha` by default, and a Workspace inside it is refused. Pair again with `--workspace` outside that directory. |
| `chat send` stops with a run waiting for approval | Answer the card in a terminal, or use `neha approval allow <approval-id>` from the ids it printed. [Approvals](approvals.md) covers it. |
| The ticket is not valid | A ticket lasts one pairing and one Space start. Restart the Space, then run `neha computer pair` again; a signed-in CLI can also ask for a ticket when no ticket file is present. |
| A read reports the path is hidden from this Computer | The path is inside the Computer's configuration directory, which granted mode hides from Bots. A Bot with full access reads it; turn full access off to hide it again. |
| A Bot's folder is refused | The daemon reports `it contains a hard link`, `it contains a symlink`, or `it contains a special file`, or `it is a symlink` or `it is not a directory` when the folder itself is the problem, or the folder exceeds its scan limits (about 10,000 entries and 8 levels deep). Delete those entries from `~/.neha/workspaces/<bot-id>`, or from your `--workspace` folder, then start `computer run` again. |
| The daemon stops after pairing | `bubblewrap` is missing or cannot start a sandbox. Install it and check the sandbox with `bwrap --ro-bind / / --dev-bind /dev /dev --proc /proc -- /bin/true`. |
| `bot create` fails because a Hosted Computer cannot start | The Space machine needs a working `bubblewrap`. Install it, or pair this machine and run `neha bot create <name> --computer local`. |
| A Hosted Bot says it could not start its Computer | `bubblewrap` on the Space machine is missing or broken. Install it and check the sandbox as above; the run fails instead of starting. |
| `computer reset` reports a run in progress | A queued, running, or waiting run blocks the reset. Wait for the run to end, then run `neha computer reset <computer-id>` again. |
| A Computer shows offline | The daemon is not running, or it lost the Space. Start it with `neha computer run` and check its terminal. A daemon from another release is refused the same way: restart it with the `neha computer run` of this release. |
| A write is refused | The path is outside the Bot's Workspace, the folders its owner granted, and the run's `--cwd` directory. Write inside those, grant the folder, or answer the approval card that asks for it. Full access removes the fence. |

[Building and verification](building.md#a-computer-sandbox) shows how to install `bubblewrap`.

## What this milestone does not do

- The Space listens on loopback only, so this milestone pairs the machine that runs the Space. A Computer on another machine is not available; `neha computer add` stays for that future flow.
- There is no command yet to revoke or replace a Computer's credential.
- Moving a Bot to or from a Hosted Computer is not available, and carrying its Workspace is not specified.
- A Hosted Computer runs in a sandbox on the Space's machine. A provider-backed Hosted machine, and stopping, starting, recovering, or destroying one, are not available.
- A saved Workspace is not available, so there is no restore: `computer reset` clears the active Workspace instead.
- A Hosted Computer has no screen, browser, Vault credential, or Take control yet.
- A Hosted Computer does not ask before an action yet; [Approvals](approvals.md) covers the gate for a Local Computer.
- An approval of a run no User is with does not expire yet, and a Question, a Group approval, and an independent reviewer are not available.
- The screen and process streams are not shown or carried by this channel yet.
- A Local Computer runs on Linux under bubblewrap. On Windows it runs as a
  managed Bot folder: file tools stay inside that folder, but commands run with
  the machine user's rights and are not OS-sandboxed; approvals are the command
  boundary. macOS is not supported. The desktop app starts the Local Computer
  on Linux and Windows only when **This computer** is chosen with the Local
  Computer checkbox, and only after the local Space has a signed-in session.

## File access and a run's directory

A Local Bot starts in its own Workspace and may write only there. The owner can widen that with `neha bot grant` and `neha bot full-access`:

```sh
BOT_ID=$(neha bot list | awk '$2 == "Runner" {print $1; exit}')
neha bot access "$BOT_ID"
neha bot grant "$BOT_ID" /data/shared
neha bot revoke "$BOT_ID" /data/shared
neha bot full-access "$BOT_ID" on
neha bot full-access "$BOT_ID" off
```

`neha bot grant <bot-id> <path>` adds one extra folder the Bot may write; repeat the command for more. The folder must already exist on this machine. The path is trimmed once where you type it, and a folder name that ends in a space is its own folder: `logs` and `logs ` are different. `neha bot revoke <bot-id> <path>` takes one away, and it also takes away a grant whose folder is gone, matching the path `neha bot access <bot-id>` shows. A revoke compares the path exactly as you spell it first, so keep a trailing space by quoting the path: `neha bot revoke <bot-id> '/data/logs '` takes the grant for `/data/logs ` and leaves `/data/logs` alone. `neha bot access <bot-id>` shows what is in effect: `full access: on` or `full access: off`, then one `grant` line per folder, or `grants: none`. Only the Bot's owner can change this. Each run fixes the access when it starts, so a change made while a run works applies to the next run.

`neha bot full-access <bot-id> on` gives the Bot the daemon process's own file rights and starts no sandbox for it, so the Bot writes and deletes what the machine's user can and its shell sees the machine's own `/tmp`, `/dev`, and process view. The risk it prints names what that reaches: the Bot can read the Computer's configuration directory, `~/.config/neha` by default, which holds the saved pairing, the machine's token, and the CLI's session credential. Turn full access on only when the Bot needs that range; `off` restores the fence. `on` asks for confirmation at a terminal and needs `--yes` without one; `off` asks for nothing.

A record that turned full access on before the owner's answer was versioned replays as granted, so a Space updated from an older build shows the Bot as granted. The owner turns it on again with `neha bot full-access <bot-id> on --yes`, which records the new answer.

In granted mode on Linux a `write_file` action that would write outside the Bot's Workspace, the granted folders, and the run's `--cwd` directory makes the run wait for the owner's approval instead of running the action; the Space asks only when it can locate the target path. Allow runs that action once; Always allow grants the folder for later runs. Deny stops the run. A shell that writes outside the fence is refused rather than turned into a request, because the Space does not know the Local Workspace's absolute path. A shell mounted into a grant or into the run's `--cwd` directory is not writable when that folder contains a hard link, symlink, or special file, so it cannot change a file outside it. The approval gate still applies with full access: the first use of the Computer asks, and an Ask rule still asks, but full access raises no folder card. On Windows the shell is not fenced; approvals are the command boundary, and grants, full access, and `--cwd` are unavailable.

A run can start its actions in another directory with `neha chat send --cwd`:

```sh
neha chat send --bot "$BOT_ID" --cwd /data/project "review the notes here"
```

On a Local Computer, `--cwd` names the directory the run starts in; without it the run starts in the Bot's Workspace. A Hosted Computer runs only in the Bot's own Workspace, and a run that names another directory fails. The directory must be absolute, must already exist, and must be a folder. A directory whose name ends in a space is typed with a trailing `/.`, which folds away and keeps the space: `--cwd '/data/logs /.'` records `/data/logs `. A path that ends in the space itself is trimmed and names the folder next to it.

The directory is that run's write root: the Bot's shell and its file tools may write inside it for that run, and it grants nothing to a later run. The directory is recorded with the run and appears on an approval card. In granted mode the Computer refuses a run whose directory sits inside a hidden root or holds one. The shell mounts an outside write root as writable only when its tree holds no link or special file and stays inside the scan limits; the file tools check every write path. Reading still follows the machine user's rights, except in granted mode, where the Computer's configuration directory stays hidden.
