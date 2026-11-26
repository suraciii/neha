# CLI

[product.md](product.md) states what the CLI does for a user or an agent. This page states the one surface through which every caller reaches a Space.

## Authority

Owned by cli. Every topic's behavior is reachable through the command surface, so every topic depends on this page for how it is reached. No topic depends on it for what it does.

## Laws

1. The GUI, the CLI, and a Bot's loop reach a Space through the command surface for every action except the transfer of a File's bytes.
2. A command runs once, however many times its caller sends it.
3. A caller and a Space agree on a version before the first command.
4. A command, its events, and the work it starts are stored together or not at all.
5. A command names a File by its id, and a File's bytes travel over HTTP beside the command surface.
6. A command that starts a run confirms when the Space queues the run, and its first result names the run and the place it holds.

## The command surface

The command surface is the set of commands a Space accepts and the events it sends. One definition of it produces the Space's types and the clients' types.

## The computer daemon

A real Computer is reached through the CLI: the binary that runs `neha computer pair` is the daemon that `neha computer run` starts.

The daemon listens on no network port. It holds one outgoing connection and no record, and it runs the actions the Space sends with the machine's rights.

The `neha computer add` and `neha computer list` commands, and `neha bot create --computer`, are part of the command surface.

## Commands and events

A command carries an identifier from its caller, the scope it acts in, and its parameters. A caller that sends a command again receives the result of the first one, or a conflict. The result of a run-starting command settles when its run ends; a caller that sends the command again before then waits for the settled result.

A command is accepted when the Space holds it, committed when the record holds it, and confirmed when its effect outside the Space is done. That effect is the run's place in the queue when the command starts a run. Each is a separate event to the caller.

## Versions

Within one major version, a newer Space accepts an older caller.

A caller and a Space tell each other what they can do. A missing capability that is optional fails only the command that needs it. A missing capability that protects data or authority ends the connection and names the upgrade.

## Streams

A client reads events from a cursor and resumes from it after a reconnect. A cursor the Space can no longer serve gives the client a fresh snapshot.
