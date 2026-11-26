# Add a real Computer

This guide connects a Linux machine to a Space as a real Computer. A real Computer runs Neha as its own process, connects out to the Space, and runs each Bot's actions in a folder of its own on the machine. It follows [Quick start](quickstart.md), so the Space, its Admin, a Provider, and a Chat already exist.

This milestone pairs a machine on the same host as the Space, because the Space listens only on loopback. [What this milestone does not do](#what-this-milestone-does-not-do) names that and the other limits.

## What you need

- A Linux machine with the Neha CLI. [Building and verification](building.md) covers the build; the examples use `target/debug/neha`.
- `bubblewrap` on that machine. The daemon runs every Bot command in a sandbox it starts.
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

The code is valid for 10 minutes. It admits one Computer. Pass `--workspace <directory>` to put the Bot folders somewhere other than the default. Set `NEHA_SPACE_URL` to leave out `--space`.

In a second terminal, enter the code from the signed-in CLI:

```bash
target/debug/neha computer add <code>
```

The Space records a real Computer owned by that User. The first terminal then writes the machine's configuration and starts the daemon, which connects out to the Space; keep that terminal open.

The machine keeps its configuration in `~/.neha/computer.json` with mode 0600, and its Bot folders under `~/.neha/workspace` by default. Set `NEHA_COMPUTER_HOME` to move both. Later starts use the saved pairing:

```bash
target/debug/neha computer run
```

## Create a Bot on the machine

List the Computers you own:

```bash
target/debug/neha computer list
```

Each line names one Computer, its kind, whether it is online, and its workspace. Then create a Bot on it:

```bash
target/debug/neha bot create Runner --computer <computer-id>
```

The output names the new Bot, its Chat, and the Computer. Without `--computer`, the Bot goes on a managed Computer, as before.

Ask the Bot to work and check where the work ran:

```bash
target/debug/neha chat send --bot <bot-id> "pwd; echo hi > a.txt"
```

## The Bot's folder and the write fence

The machine's Workspace is the folder passed as `--workspace`, or `~/.neha/workspace` by default. Each Bot has a folder of its own inside it, named by the Bot's id:

```text
~/.neha/workspace/<bot-id>/
```

The folder is created when the Bot first runs an action. Every action starts in that folder: a relative path names a file inside it, and `pwd` prints it.

A Bot writes only inside its own folder. A write to any other path fails with `the path is outside this Bot's writable folder`. A path that climbs out with `..` or uses a symlink to leave the folder is refused, and the refused write creates no file.

A Bot reads with the rights of the machine's user. It can read any file that user can read, including files outside its folder and files of another Bot on the same machine; those other files stay read-only for it.

The fence is enforced by the operating system, not only by the tools. A shell command runs under `bubblewrap`, which mounts the machine read-only and the Bot's folder writable, so a command cannot write outside the folder either.

A command also gets a fresh `/tmp` inside the sandbox. Files written there are gone when the command ends. A machine whose `bubblewrap` cannot start a sandbox does not serve as a Computer.

## While the machine is offline

A Computer is online while its daemon holds the channel to the Space. If the daemon stops, the Computer shows as offline in `neha computer list`, and a Message sent to one of its Bots waits in the record. `neha chat send` keeps waiting while the run stays queued.

When the daemon starts again with `neha computer run`, it reconnects, queued runs start, and the waiting command prints the reply.

If the connection drops while an action runs, the Space records that action as unknown and fails the run. The Bot tells the Chat that it lost its Computer. The run does not resume.

## Troubleshooting

| Symptom | What to do |
| --- | --- |
| `pair` asks for a Space URL | Pass `--space <url>`, or set `NEHA_SPACE_URL`. |
| `pair` stops before printing a code | The machine cannot reach the Space URL. Check the address and that the Space runs. |
| `add` reports the code is not valid or has expired | The code lasts 10 minutes and admits one Computer. Run `pair` again for a new code. |
| The daemon stops after pairing | `bubblewrap` is missing or cannot start a sandbox. Install it and check the sandbox with `bwrap --ro-bind / / --dev-bind /dev /dev --proc /proc -- /bin/true`. |
| A Computer shows offline | The daemon is not running, or it lost the Space. Start it with `neha computer run` and check its terminal. |
| A write is refused | The path is outside the Bot's folder. Write inside the folder, or use a relative path. |

[Building and verification](building.md#a-real-computer-sandbox) shows how to install `bubblewrap`.

## What this milestone does not do

- The Space listens on loopback only, so this milestone pairs a machine on the host that runs the Space. Remote pairing needs a Space reachable over the network.
- Granted folders and the `full-access` switch are not available. A Bot writes only its own folder, and its owner cannot widen that range.
- A request cannot choose a working directory. Every run starts in the Bot's folder.
- There is no approval gate. A Bot runs an action as soon as the Model asks for it.
- The screen and process streams are not shown or carried by this channel yet.
- A run interrupted by a lost connection fails and does not resume.
- Only Linux is supported for a real Computer; the desktop app does not turn a machine into a Computer yet.
