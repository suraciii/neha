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

The browser client reads and writes through the command surface, as the CLI does.

## Management

Management is what a signed-in User changes on the Space: its Providers and Models, a Bot and its memory, the access and Workspace of that Bot's Computer, and the rules. An authenticated browser session and the CLI reach the same management commands.

The browser's Bot page, Settings, and Computers list issue the same commands the CLI issues for a Bot, its memory, a Model, and a Computer. Turning full access on means the same consent in both.

## A Bot's shell

A run's shell gets `NEHA_BOT_TOKEN`, `NEHA_SPACE_URL`, and `NEHA_CHAT`. The token is bound to that run's Bot and Chat. It lives only in the Space's memory and expires; a restart drops it and a later run issues a new one.

With the token, `neha file get <id>` and `neha chat send --file <path>` move a File's bytes. No other command is open to it.

## The computer daemon

A Local Computer is reached through the CLI: the binary that runs `neha computer pair` is the daemon that `neha computer run` starts.

A `--space` that names a non-loopback origin pairs through the Space's code flow: the command starts the pairing, prints the Space origin and code, and polls until a signed-in User approves it. It never sends a local ticket or a saved session token to that origin.

The daemon holds no record and runs the actions the Space sends with the machine's rights. [Computer](../computer/design.md#the-channel) covers the connection it makes and what crosses it.

The `neha computer add`, `neha computer list`, `neha computer reset`, `neha bot create --computer`, and `neha bot move` commands are part of the command surface, and so are `neha approval` and `neha rule`.

## Commands and events

A command carries an identifier from its caller, the scope it acts in, and its parameters. A caller that sends a command again receives the result of the first one, or a conflict. The result of a run-starting command settles when its run ends; a caller that sends the command again before then waits for the settled result.

A command is accepted when the Space holds it, committed when the record holds it, and confirmed when its effect outside the Space is done. That effect is the run's place in the queue when the command starts a run. Each is a separate event to the caller.

## Versions

Within one major version, a newer Space accepts an older caller.

A caller and a Space tell each other what they can do. A missing capability that is optional fails only the command that needs it. A missing capability that protects data or authority ends the connection and names the upgrade.

A caller that names no Presentation kinds reads every kind its protocol version already carries; a later Presentation kind reaches only a caller that names it.

A caller that does not name a later kind receives no frame for it, and its cursor advances past it. An older handshake still receives `neha.message.added` and the other kinds its version carries, and it skips only the new ones.

A caller declares the Presentation event kinds it reads as a capability, and the command surface defines the capability names and the fixed set of kinds each one admits. `memory` admits `neha.memory.added`, `neha.memory.removed`, and `neha.memory.answered`; `compaction` admits `neha.chat.compacted`; `plan` admits `neha.run.planned`. A capability admits no kind added after it was published; a later kind needs a new capability name.

Turning full access on needs the consent version the Space states. An older caller that sends none is refused, and nothing is recorded.

A request that names a working directory carries its write authorization, so a client written before the marker existed is refused and told to upgrade. The CLI sends the marker with `--cwd`; the browser composer names no directory.

## Streams

A client reads events from a cursor and resumes from it after a reconnect. A cursor the Space can no longer serve gives the client a fresh snapshot.
