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

A machine joins a Space by pairing; [Identity](../identity/design.md#codes) covers the local ticket and the token the machine keeps.

The channel carries commands, file reads and writes, process streams, and the screen. A File's bytes cross it as their own exchange, and a Hosted Computer uses the same channel on the Space's own machine.

The Space sends each action with an id, and the Computer answers that id with the action's result. Each action has a time limit, including one that moves a File's bytes, and an action that runs past its limit ends as timed out.

A connection that ends makes every action in flight unknown. The process reconnects while it runs, and the Space shows the Computer offline until it does.

## The machine

A Local Computer runs on the machine where Neha runs, which its owner paired. A Hosted Computer runs as a machine a provider starts.

A provider implements four operations as one contract: start and stop a machine, show its screen, run a process on it, and read and write its files.

The record names a Computer's kind and a provider's machine. That machine reference only speeds up a reconnect; it never holds durable data.

## The workspace

A Hosted Computer keeps each Bot's Workspace in the Space. A Local Computer keeps each Bot's Workspace on the machine, under `~/.neha/workspaces/<bot-id>/` by default, and the Space keeps no copy.

A Local Computer keeps its Bots' Workspaces under one root, and the machine's owner can choose another root for it. Each Bot has its own Workspace there, and the actions of a run start in it.

A Local pairing MAY record no Workspace root; the root its daemon serves is the one its runs use.

## Moving a Bot

A move is recorded as an Authority event. A run fixes the Bot's Computer when it starts, so a run already under way ends on the old Computer, and a run that starts after the move uses the new one.

Working files do not migrate. The old Computer keeps them, and the new Computer gives the Bot a Workspace of its own.

## Isolation

A Computer is the unit of isolation.

Bots assigned to one Computer share its files, processes, and logins. The Space does not isolate them from each other.

On a Local Computer, a Bot's default write range is its own Workspace, and it reads with the machine user's rights. The file tools check every write path, and the shell runs in an operating-system sandbox that mounts the machine read-only except that Workspace. A machine that cannot start its sandbox does not serve as a Computer.

A Local Computer's configuration directory is hidden from its Bots. It holds the saved pairing and the token, at `~/.config/neha` by default, and the file tools and the shell cannot reach it.

On a Local Computer, a Bot's Workspace is checked before it is served: a hard link, a symlink, and a special file are not accepted.

A Local Computer's owner grants what a Bot on it can reach. [product.md](product.md) covers the grants.

## Lifecycle

A Computer is created, started, stopped, recovered, reset, and destroyed.

Stopping a Hosted Computer saves its Bots' Workspaces and takes the machine offline. Starting brings a Computer online again.

Recovery first saves what it can from the machine, then builds each Bot's Workspace from its last save. Reset builds each Bot's Workspace from its last save and saves nothing.

A run stops between two actions before saved Workspaces are replaced, and the Bot says when work was interrupted.

Destroying a Computer deletes its saved Workspaces.

## Saving a workspace

The Space saves a Hosted Bot's Workspace when a run completes or fails, before a stop, and before a stop that follows inactivity.

A save skips rebuildable paths: caches, package stores, dependency directories, build output, and logs.

A Bot's Workspace over the Space's size limit is not saved. The User is told, and the work stays on the machine.

A restore accepts only relative paths of directories and regular files, and writes nothing outside the workspace.

Saves of one Computer happen one at a time. A run that starts during a save waits for it.

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
