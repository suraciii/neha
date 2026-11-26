# Add a real Computer

This guide connects a Linux machine to a Space as a real Computer. A real Computer runs Neha as its own process, connects out to the Space, and runs each Bot's actions in that Bot's Workspace on the machine. It follows [Quick start](quickstart.md), so the Space, its Admin, a Provider, and a Chat already exist.

This milestone pairs a machine on the same host as the Space, because the Space listens only on loopback. [What this milestone does not do](#what-this-milestone-does-not-do) names that and the other limits.

## What you need

- A Linux machine with the Neha CLI. [Building and verification](building.md) covers the build; the examples use `target/debug/neha`.
- `bubblewrap` on that machine. The daemon runs each shell command in a sandbox it starts; the file tools check their own paths.
- A Space that the machine can reach at the URL you pair with; in this milestone that is the host that runs the Space.
- A CLI signed in to the Space. [Quick start](quickstart.md#join-as-the-admin-from-the-cli) covers `neha join`.

## Pair the machine

Start the machine's process in its own terminal:

```bash
target/debug/neha computer pair --space http://127.0.0.1:7337
```

The command prints a pairing code and waits:

```text
Pairing code: <code>
```

The code is valid for up to 10 minutes. It admits one Computer. The Bot Workspaces go in `~/Neha` by default; pass `--workspace <directory>` to move them, and a directory inside the configuration directory is refused. Set `NEHA_SPACE_URL` to leave out `--space`.

In a second terminal, enter the code from the signed-in CLI:

```bash
target/debug/neha computer add <code>
```

The command prints the new Computer's id, kind, and workspace. The first terminal then writes the machine's configuration and starts the daemon, which connects out to the Space; keep that terminal open.

The machine keeps its configuration in `~/.neha/computer.json` with mode 0600, and the Bots' Workspaces in `~/Neha` by default. Set `NEHA_COMPUTER_HOME` to move the configuration; `--workspace` chooses where the Workspaces live. Later starts use the saved pairing:

```bash
target/debug/neha computer run
```

A saved pairing whose Workspace is inside the configuration directory is refused. Run `computer pair` again, or pair with `--workspace` outside that directory.

Re-pairing adds a new Computer; the Bots on the old one stay bound to it. Their Workspaces and any queued work stay behind, and nothing moves automatically. Create a new Bot on the new Computer with `neha bot create --computer`, or wait for the capability that changes a Bot's Computer.

## Create a Bot on the machine

List the Computers you own:

```bash
target/debug/neha computer list
```

Each line gives one Computer's id, kind, online state, and workspace. The list holds managed Computers as well; `bot create --computer` accepts only a Computer whose kind is `real`:

```bash
target/debug/neha bot create Runner --computer <computer-id>
```

The output names the new Bot, its Chat, and the Computer. Without `--computer`, the Bot goes on a managed Computer, as before.

Ask the Bot to work and check where the work ran:

```bash
target/debug/neha chat send --bot <bot-id> "pwd; echo hi > a.txt"
```

## The Bot's Workspace and the write fence

The `--workspace` directory, `~/Neha` by default, holds one Workspace folder for each Bot, named by the Bot's id:

```text
~/Neha/<bot-id>/
```

The folder is created when the Bot first runs an action. Every action of the Bot starts in its Workspace: a relative path names a file inside it, and `pwd` prints it.

The file tools check every write path. A write outside the Workspace fails with `the path is outside this Bot's writable folder`; a `..` climb or a symlink out is refused, and no file is created. The only persistent place a Bot writes on the machine is its Workspace.

The file tools read with the rights of the machine's user. A file read follows a symlink as the machine user's own tools do, and reaches any file that user can read, including files outside the Workspace and files of another Bot on the machine. A directory listing does not follow a symlink that is the path itself, and a FIFO or a device is not read.

The Computer's own configuration directory is the exception. A path that resolves into it is refused, even through a symlink; the daemon hides it from the file tools and the shell, so a Bot cannot read the saved pairing or its token.

The same split reaches Files: `take_file` copies a File of the Chat into the Workspace, and `put_file` may read any file the machine's user can read, outside the configuration directory, and deliver it as a File on a Message. [Send and receive Files](files.md#let-the-bot-take-and-deliver) covers both tools.

A shell command runs in an operating-system sandbox: the machine is read-only, the Bot's Workspace is writable, and an empty tmpfs covers the Computer's configuration directory. A command also gets a fresh `/tmp` inside the sandbox; files written there are gone when the command ends and never reach the machine's own `/tmp`.

The daemon checks a Bot's folder before it serves it. A hard link, a symlink, or a special file in the folder makes the daemon refuse that folder until it is cleaned; keep links and special files out of the Workspace.

A machine whose `bubblewrap` cannot start a sandbox does not serve as a Computer.

## While the machine is offline

A Computer is online while its daemon holds the channel to the Space. If the daemon stops, the Computer shows as offline in `neha computer list`, and a Message sent to one of its Bots waits in the record.

`neha chat send` waits up to 180 seconds for the run to end. If the run is still queued, the command stops with `the run did not end within 180s`, and the Message and its run stay in the record.

When the daemon starts again with `neha computer run`, it reconnects and queued runs start. Read the Chat later for the reply; do not send the same Message again. `neha chat list` names the Chats, and `neha chat read <chat-id>` shows their Messages.

If the connection drops while an action runs, the Space records that action as unknown and fails the run. The Bot tells the Chat that it lost its Computer. The run does not resume.

The machine ends the command it was running when the channel drops, so no action keeps running unattended.

## Troubleshooting

| Symptom | What to do |
| --- | --- |
| `pair` asks for a Space URL | Pass `--space <url>`, or set `NEHA_SPACE_URL`. |
| `pair` stops before printing a code | Read the error it prints. A workspace directory it cannot create and a Space it cannot reach are the common causes. |
| `pair` or `computer run` reports the workspace is inside the configuration directory | The configuration directory is `~/.neha` by default, and a Workspace inside it is refused. Run `computer pair` again, and pass `--workspace` outside that directory if you want a different place. |
| A read reports the path is hidden from this Computer | The path is inside the Computer's configuration directory. A Bot cannot read that directory. |
| A Bot's folder is refused | The daemon reports `it contains a hard link`, `a symlink`, or `a special file`, or the folder exceeds its scan limits (about 10,000 entries and 8 levels deep). Delete those entries from `~/Neha/<bot-id>`, or from your `--workspace` folder, then start `computer run` again. |
| `add` reports the code is not valid or has expired | The code lasts up to 10 minutes and admits one Computer. Run `pair` again for a new code. |
| The daemon stops after pairing | `bubblewrap` is missing or cannot start a sandbox. Install it and check the sandbox with `bwrap --ro-bind / / --dev-bind /dev /dev --proc /proc -- /bin/true`. |
| A Computer shows offline | The daemon is not running, or it lost the Space. Start it with `neha computer run` and check its terminal. |
| A write is refused | The path is outside the Bot's Workspace. Write inside the Workspace, or use a relative path. |

[Building and verification](building.md#a-real-computer-sandbox) shows how to install `bubblewrap`.

## What this milestone does not do

- The Space listens on loopback only, so this milestone pairs a machine on the host that runs the Space. Remote pairing needs a Space reachable over the network.
- Granted folders and the `full-access` switch are not available. A Bot's write range stays its own Workspace, and its owner cannot widen that range.
- A request cannot choose a working directory. Every run starts in the Bot's Workspace.
- A Bot cannot change its Computer. It stays on the Computer it was created on.
- There is no approval gate. A Bot runs an action as soon as the Model asks for it.
- The screen and process streams are not shown or carried by this channel yet.
- A run interrupted by a lost connection fails and does not resume.
- Only Linux is supported for a real Computer; the desktop app does not turn a machine into a Computer yet.
