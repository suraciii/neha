# Add a Local Computer

This guide turns the machine that runs a Space into a Local Computer: a Neha process that connects out to the Space and runs each Bot's actions in that Bot's Workspace on the machine. It follows [Quick start](quickstart.md), so the Space, its Admin, a Provider, and a Chat already exist.

This milestone pairs the machine that runs the Space, because the Space listens only on loopback. [What this milestone does not do](#what-this-milestone-does-not-do) names that and the other limits.

## What you need

- A Linux machine with the Neha CLI. [Building and verification](building.md) covers the build; the examples use `target/debug/neha`.
- `bubblewrap` on that machine. The daemon runs each shell command in a sandbox it starts; the file tools check their own paths.
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

The ticket admits one pairing for one Space start and expires after 10 minutes. A Space that started with an Admin writes its startup ticket as one file per Space under the machine's Neha configuration directory, with mode 0600, where Bots cannot read it.

Each Space's ticket sits at `$XDG_CONFIG_HOME/neha/spaces/<space-key>/local-computer.ticket`, or `~/.config/neha/spaces/<space-key>/local-computer.ticket` when `XDG_CONFIG_HOME` is not set. `computer pair` scans `spaces/` and pairs the Space whose URL its ticket names; `--ticket-file` names another file. When no ticket file exists, a signed-in CLI asks the Space that saved its session for one, so only that fallback needs a signed-in CLI.

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

Each line gives one Computer's id, kind, online state, and workspace. The kind is `local` for a paired machine and `hosted` for one the Space runs. The pairing printed the new Computer's id; create a Bot on it:

```bash
target/debug/neha bot create Runner --computer <computer-id>
```

`--computer` takes `local`, `hosted`, or a Computer id:

| Value | What happens |
| --- | --- |
| `local` | The owner's Local Computer; with several, the earliest one recorded, which may be an old or offline machine |
| `hosted` | A new Hosted Computer, one per Bot |
| A Computer id | The Local Computer with that id |

A Local Computer's workspace column is empty when the Space recorded no path for it; a Local Computer paired with `computer pair` records none, and `neha bot show` then omits its Workspace line. The root that `neha computer run` prints as `Workspace root:` is the one its actions use.

Without `--computer`, a new Bot starts on your Local Computer when one is paired, and on a Hosted Computer otherwise; the target default is Hosted. A Hosted Computer belongs to its Bot, so naming its id is refused; use `--computer hosted` for a new one.

Ask the Bot to work and check where the work ran:

```bash
target/debug/neha chat send --bot <bot-id> "pwd; echo hi > a.txt"
```

The first action on this Local Computer asks for approval in the Chat: the Bot waits until you Allow or Deny, and the first use has no Always allow. Each Local Computer asks once for each Bot and User, so a Bot moved to another Local Computer asks again there. [Approvals](approvals.md) covers the card and the commands.

## The Bot's Workspace and the write fence

The Workspace root is `~/.neha/workspaces` by default; `--workspace` on `computer pair` chooses another root. The root holds one folder for each Bot, named by the Bot's id:

```text
~/.neha/workspaces/<bot-id>/
```

The folder is created when the Bot first runs an action. Every action of the Bot starts in its Workspace: a relative path names a file inside it, and `pwd` prints it. The `Workspace root:` line from `neha computer run` names the root the daemon actually serves.

The file tools check every write path. A write outside the Workspace fails with `the path is outside this Bot's writable folder`; a `..` climb or a symlink out is refused, and no file is created. The only persistent place a Bot writes on the machine is its Workspace.

The file tools read with the rights of the machine's user. A file read follows a symlink as the machine user's own tools do, and reaches any file that user can read, including files outside the Workspace and files of another Bot on the machine. A directory listing does not follow a symlink that is the path itself, and a FIFO or a device is not read.

The Computer's own configuration directory is the exception. A path that resolves into it is refused, even through a symlink; the daemon hides it from the file tools and the shell, so a Bot cannot read the saved pairing or its token. The directory is `~/.config/neha` by default.

The same split reaches Files: `take_file` copies a File of the Chat into the Workspace, and `put_file` may read any file the machine's user can read, outside the configuration directory, and deliver it as a File on a Message. Both tools go through the approval gate like other actions. [Send and receive Files](files.md#let-the-bot-take-and-deliver) covers both tools.

A shell command runs in an operating-system sandbox: the machine is read-only, the Bot's Workspace is writable, and an empty tmpfs covers the Computer's configuration directory. A command also gets a fresh `/tmp` inside the sandbox; files written there are gone when the command ends and never reach the machine's own `/tmp`.

The daemon checks a Bot's folder before it serves it. A hard link, a symlink, or a special file in the folder makes the daemon refuse that folder until it is cleaned; keep links and special files out of the Workspace.

A machine whose `bubblewrap` cannot start a sandbox does not serve as a Computer.

## Moving a Bot

Move a Bot from one of your Local Computers to another:

```bash
target/debug/neha bot move <bot-id> --computer <computer-id>
```

The command prints the Computer the Bot now works on. The Bot's memory, its Chats, and the Files it delivered stay with it.

The old Computer keeps the Bot's folder, so work the Bot did not deliver stays there. Moving does not copy files: the Bot's first action on the new Computer creates its folder when it does not exist; when it does exist, for example when a second Computer on the same machine reuses one `--workspace`, the Bot uses the files already there. To show an empty folder on one machine, give the new Computer its own `--workspace`.

A run already under way finishes on the old Computer. A run that starts after the move uses the new Computer, including a request that was still waiting in the queue.

The old Computer may be offline when you move the Bot. If the new Computer is offline too, the move is still recorded, and waiting work runs when it comes back. A Bot on a Hosted Computer cannot move: create a new Bot on the target Computer instead.

## While the machine is offline

A Computer is online while its daemon holds the channel to the Space. If the daemon stops, the Computer shows as offline in `neha computer list`, and a Message sent to one of its Bots waits in the record.

`neha chat send` waits up to 180 seconds for the run to end. If the run is still queued, the command stops with `the run did not end within 180s`, and the Message and its run stay in the record.

When the daemon starts again with `neha computer run`, it reconnects and queued runs start. Read the Chat later for the reply; do not send the same Message again. `neha chat list` names the Chats, and `neha chat read <chat-id>` shows their Messages.

If the connection drops while an action runs, the Space records that action as unknown and fails the run. The Bot tells the Chat that it lost its Computer. The run does not resume.

If the Space itself stops while a run works, the next start continues the run from its record. Completed steps replay into the Model's context; an action that started and never returned is checked, not run again: a `write_file` counts as done only when the file holds exactly the requested content, and any other unfinished action makes the run fail with its result unknown. A step whose Model reply never arrived is called again from its start; that call may be billed twice even though the Space did not keep its usage. A recovered continuation can still fail if its Provider is unavailable. The Bot answers once, and no recorded action runs twice.

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
| A read reports the path is hidden from this Computer | The path is inside the Computer's configuration directory. A Bot cannot read that directory. |
| A Bot's folder is refused | The daemon reports `it contains a hard link`, `a symlink`, or `a special file`, or the folder exceeds its scan limits (about 10,000 entries and 8 levels deep). Delete those entries from `~/.neha/workspaces/<bot-id>`, or from your `--workspace` folder, then start `computer run` again. |
| The daemon stops after pairing | `bubblewrap` is missing or cannot start a sandbox. Install it and check the sandbox with `bwrap --ro-bind / / --dev-bind /dev /dev --proc /proc -- /bin/true`. |
| A Computer shows offline | The daemon is not running, or it lost the Space. Start it with `neha computer run` and check its terminal. |
| A write is refused | The path is outside the Bot's Workspace and the folders its owner granted. Write inside those, grant the folder, or answer the approval card that asks for it. |

[Building and verification](building.md#a-local-computer-sandbox) shows how to install `bubblewrap`.

## What this milestone does not do

- The Space listens on loopback only, so this milestone pairs the machine that runs the Space. A Computer on another machine is not available; `neha computer add` stays for that future flow.
- There is no command yet to revoke or replace a Computer's credential.
- Moving a Bot to or from a Hosted Computer is not available, and carrying its Workspace is not specified.
- A Hosted Computer runs inside the Space's process; a provider-backed Hosted machine is not available.
- A Hosted Computer does not ask before an action yet; [Approvals](approvals.md) covers the gate for a Local Computer.
- An approval of a run no User is with does not expire yet, and a Question, a Group approval, and an independent reviewer are not available.
- The screen and process streams are not shown or carried by this channel yet.
- Only Linux is supported for a Local Computer; the desktop app does not turn a machine into a Computer yet.

## File access and a run's directory

A Local Bot starts in its own Workspace and may write only there. The owner can widen that with `neha bot update`:

```sh
neha bot update <bot-id> --grant /data/shared
neha bot update <bot-id> --full-access on
neha bot update <bot-id> --clear-grants
```

`--grant` adds one extra folder the Bot may write; repeat it for more. The folder must already exist on this machine. `--full-access on` gives the Bot the daemon process's own write rights and removes the write fence, so its shell sees the machine's own `/tmp`, `/dev`, and process view; `off` restores the fence. Only the Bot's owner can change this, and the change applies to the next run, not one already running.

When a Bot's action would write outside its Workspace and the granted folders, the run waits for the owner's approval instead of running the action. This covers a file path the Space can see and a shell that names a working directory. A shell that writes from the default Workspace is refused by the fence rather than turned into a request, because the Space does not know the Local Workspace's absolute path; grant the folder, or name it with `--cwd`, to let the Bot ask. A shell mounted into a grant is not writable when the grant contains a hard link, symlink, or special file, so it cannot change a file outside the grant. Allow runs that action once; Always allow grants the folder for later runs. Deny stops the run. The Computer's configuration directory stays hidden even with full access.

A run can start its actions in another directory with `neha chat send --cwd`:

```sh
neha chat send --bot <bot-id> --cwd /data/project "review the notes here"
```

Without `--cwd` the run starts in the Bot's Workspace. The directory is recorded with the run and appears on an approval card. It must be absolute. Choosing a directory does not make it writable: writes still need the Workspace, a grant, or full access. Reading still follows the machine user's rights, except the Computer's own configuration directory, which stays hidden.
