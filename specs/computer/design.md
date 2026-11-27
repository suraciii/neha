# Computer

[product.md](product.md) states what a Computer is for a User. This page states how a Computer connects to the Space and what passes between them.

## Authority

Owned by computer. A Computer's channel depends on [Space](../space/product.md) for the connection it makes and on [Identity](../identity/product.md) for the pairing that admits it.

A Hosted Computer depends on [Space](../space/product.md) for the workspace it keeps.

## Laws

1. A Computer connects to the Space; the Space never connects to a Computer.
2. A Computer offers only what its owner granted.
3. A credential from the Vault reaches a Computer only when a Bot uses it.
4. A tool a Bot uses runs on the Bot's Computer.
5. One actor controls a screen at a time.
6. A Computer's control and its data share one outbound connection, and its screen never shares it.
7. The end of the control stream to the Space ends every process the Computer started during it, except a process a Bot started in background mode.
8. A Bot's command runs on pipes with its output and error kept apart, and ending it ends its whole process tree.
9. A workspace holds no record, memory, or credential.
10. The Space holds every File, and a lost Workspace loses no File.

## The channel

A Local Computer runs Neha as a persistent process. The process holds one outgoing connection to the Space for control and data, and one more per screen; it listens on no network port.

A machine joins a Space by pairing; [Identity](../identity/design.md#codes) covers the local ticket, pairing code, and the token the machine keeps. A Space-provisioned Hosted Computer worker receives its token from the Space and opens the same channel.

The channel carries commands, file reads and writes, process streams, and the screen. A File's bytes cross it as their own exchange. A Hosted Computer runs its actions in an environment the Space provides; a worker-backed or provider-backed Hosted Computer uses the same channel.

The Space sends each action with an id, and the Computer answers that id with the action's result. Each action has a time limit, including one that moves a File's bytes, and an action that runs past its limit ends as timed out.

A connection that ends makes every action in flight unknown. The process reconnects while it runs, and the Space shows the Computer offline until it does.

The greeting a Computer sends when its channel opens states whether its machine serves granted folders, whether it serves a Bot's full access, and whether it fences a Bot's run. A greeting that names neither takes both from its `sandbox` flag: a machine that fences its Bots serves both, and one that does not serves neither.

The Space keeps the statement after the Computer's channel drops, and its view carries it. Before a Computer's first greeting in this Space's process the Space knows nothing about its access, and shows and allows none of it. A client offers only the access the statement names, and a command that widens access past it is refused.

The view also names whether the machine fences a Bot's run, and it names nothing at all before the first greeting. A Computer that has not answered is not a Computer without a fence, so a client that knows nothing says it waits and claims nothing about what a Bot's shell runs as.

## The machine

A Local Computer runs on a machine its owner controls. A Hosted Computer runs on the Space's machine, in a process-local sandbox or in a worker container.

A worker-backed Hosted Computer runs as a container on the Space's machine. It connects out over the same channel, mounts only its Workspace, and listens on no network port. The container is its isolation boundary, so it starts no nested sandbox.

A provider-backed Hosted Computer would run as a machine a provider starts, with four operations: start and stop a machine, show its screen, run a process on it, and read and write its files. That form is not specified yet.

The record names a Computer's kind. A provider-backed Computer would also name its machine, and that reference would only speed up a reconnect; it would never hold durable data.

## The workspace

A Hosted Computer keeps each Bot's Workspace in the Space, whether its actions run in the Space process or in a worker container. A restart of the Space keeps it. A Local Computer keeps each Bot's Workspace on the machine, under `~/.neha/workspaces/<bot-id>/` by default, and the Space keeps no copy.

A Local Computer keeps its Bots' Workspaces under one root, and the machine's owner can choose another root for it. Each Bot has its own Workspace there, and the actions of a run start in it.

A Local pairing MAY record no Workspace root; the root its daemon serves is the one its runs use.

## Moving a Bot

A move is recorded as an Authority event. A run fixes the Bot's Computer when it starts, so a run already under way ends on the old Computer, and a run that starts after the move uses the new one.

Working files do not migrate. The old Computer keeps them, and the new Computer gives the Bot a Workspace of its own.

## Isolation

A Computer is the unit of isolation.

Bots assigned to one Computer share its files, processes, and logins. The Space does not isolate them from each other.

On a Local Computer with granted folders, a Bot's write range is its own Workspace and the granted folders, and it reads with the machine user's rights. The file tools check every write path. On Linux, the shell runs in an operating-system sandbox that mounts the machine read-only except those write roots.

On Windows, an explicitly opted-in Local Computer has no operating-system sandbox. Its file tools stay in the Bot's folder, but its shell runs with the machine user's rights.

On a Hosted Computer in the Space process, a Bot's shell runs in a sandbox that starts from an empty root. The sandbox mounts the Space machine's system directories read-only, and the Bot's Workspace at `/workspace` is the one place where files persist.

On a Hosted Computer in a worker container, the shell runs in that container. The container starts no nested sandbox; the worker's container is the fence. The Workspace is the only persistent writable mount.

The sandbox has its own process list, `/dev`, `/proc`, a fresh `/tmp`, and `/dev/shm`, and it ends with the Space process. The Space's record, Vault, File store, and another Computer's Workspace are not mounted in it.

The process-local sandbox shares the machine's network, so the Bot's shell reaches the Space's loopback for `neha file get` and `neha chat send --file`. A worker container reaches the Space over the network. The file tools stay in the Workspace, and the other writable places are the sandbox's own `/tmp` and `/dev/shm`.

A machine that cannot start its sandbox does not serve as a process-local Computer, except for an explicitly opted-in Windows Local Computer. A worker container uses its container runtime as the sandbox. The Space accepts a Windows daemon's truthful `sandbox: false` only when its operator sets `NEHA_ALLOW_UNSANDBOXED_COMPUTERS=1`.

The desktop sets the opt-in for its bundled Space after the owner accepts the warning. Such a Computer has no OS fence, and its owner MUST treat approvals and Bot trust as the boundary.

A Local Computer's configuration directory is hidden from a Bot with granted folders. It holds the saved pairing and the token, at `~/.config/neha` by default, and the file tools and the shell cannot reach it.

On a Local Computer with full access, the shell starts no sandbox, and the file tools apply no hidden path and no write fence. The Bot then reaches the configuration directory as the machine user does.

A record that widened a Bot to full access before the owner's answer was versioned replays as granted. The owner turns full access on again with an explicit answer.

On a Local Computer, a run MAY name another existing directory as its working directory. That directory is a write root for that run alone, and it grants nothing to a later run.

A grant and a run's working directory keep every component name exactly: `logs` and `logs ` are different folders.

On a Local Computer with granted folders, a Bot's Workspace is checked before it is served: a hard link, a symlink, and a special file are not accepted. A Bot with full access is served without that check.

A Local Computer's owner grants what a Bot on it can reach. [product.md](product.md) covers the grants.

## Lifecycle

A Hosted Computer is created with its Bot, and only its owner resets it. A Local Computer is paired; no command resets it.

Resetting clears every entry inside the Bot's Workspace and keeps the Workspace folder. It restores no file and writes no event. Repeating the same command returns its recorded result instead of clearing again.

A reset runs only when the Bot has no queued, running, or waiting run. No run starts while a reset is in progress.

Starting, stopping, recovering, and destroying a provider-backed Hosted Computer, and saving and restoring a Workspace, are not yet specified. A worker container follows the same lifecycle as a Hosted Computer.

## Files

A File crosses between a Workspace and the Space only as a copy: a Bot takes bytes into its Workspace and delivers bytes out of it. A copy in a Workspace is working material, and [Space](../space/design.md#storage) holds the File. A Computer that is lost, reset, or destroyed takes no File of a conversation with it.

## Online and offline

A Computer is online while its channel is up. The Space shows it as offline after its channel has been down for longer than a set time.

## Tools

A tool that runs as its own process runs on the Computer as well, with what the Computer was granted.

## The screen

The Space relays a Computer's screen to its clients. Watching a screen and taking control of it are separate grants.

Taking control gives the keyboard and mouse to the User and pauses every Bot on that screen. Each Bot's other work continues.

On a Local Computer, input from its own keyboard or mouse that Neha did not inject gives control to the User. A Bot does not replay an action that this interrupted, and observes the screen again before it acts after the User is done.

## Credentials

Neha removes a credential from the Vault out of everything a Bot sends back.

On a Local Computer, a Bot uses the machine's own logins.
