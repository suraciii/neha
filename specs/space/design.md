# Space

[product.md](product.md) states what a Space is for its Users. This page states where a Space keeps its record and the bytes of its Files, how the record grows, and who can read it.

## Authority

Owned by space. Every other topic depends on a Space to hold its record.

## Laws

1. The Space is the only writer of the record.
2. A Space keeps its record in one database.
3. An event does not change once it is in the record.
4. A reader skips an event it does not understand only when its kind affects presentation alone.
5. A listener beyond the Space's own machine requires TLS or a trusted proxy that terminates it, and the Space refuses to start without either.
6. The Space checks `Host` and `Origin` before it authenticates a request, except that a request that carries a bearer token skips the `Origin` check.

## Storage

A Space keeps its record in one SQLite database on the Space's machine.

A File's bytes live in the File store, which the Space's configuration names: a directory in the Space's home, or one prefix of an S3-compatible bucket.

An object in the File store is named by its content hash, and the name is the same on every backend. The Space stores one copy of each content, and every File with that content shares it.

The Space writes a File's bytes before the record that names it. The record never names bytes the Space does not hold.

The File store only grows: the Space never overwrites an object and never deletes one.

A backup of a Space is its home directory with the objects in the File store. A restore on another machine also needs the Vault's master key.

The operator MAY configure server-side encryption with a KMS key (SSE-KMS) for the objects in an S3-compatible bucket.

Storage is one seam: above it, no directory name and no bucket name appears.

## Events

An event has a kind, a version, an author, and data. A kind belongs to the Kernel, which declares the schema of its data, and Neha rejects data that does not match it.

An installed Plugin declares no kind. [Plugin](../plugin/design.md#record) states what one can write.

A reader keeps every event. A kind it does not know that decides who can read or act makes it stop and ask for an upgrade.

## Approval events

The record holds these kinds for approvals and rules:

| Kind | What it records |
| --- | --- |
| `neha.run.reply` | A step's reply: its content, calls, and usage |
| `neha.approval.requested` | The Computer when the action runs on one and otherwise the MCP server, the call's index, the action, its arguments, the summary, the reason, the rule that asked when one did, whether an ambiguous form raised it, the lasting scope, and whether the action can be undone |
| `neha.approval.decided` | The answer, and the rule a lasting answer added |
| `neha.rule.added` | A lasting Ask or Allow rule |
| `neha.rule.removed` | A lasting rule removed |

An approval from a genuine Ask rule or a first use has `ambiguous` false, and a genuine Ask rule names the rule that asked; an ambiguous form has `ambiguous` true and an empty `rule`.

Each is an Authority event, so a reader that does not know one stops and asks for an upgrade.

## Event visibility

Realtime events and snapshots go only to the User whose account is the event's author or acting User. Space-level events (`neha.user.joined`, `neha.provider.added`) go to every account. An event the account cannot see still advances its cursor.

## Projections

A projection is built from the record and can be built again from it. Building a projection runs no tool and sends no message.

## Chat state

The Space projects one state for each Chat: working, waiting, failed, or idle. The projection reads the Chat's run and approval events and nothing else, and a restart replays it to the same state.

A queued, running, or resumed run projects working, and a run that waits for an Approval or an answer projects waiting. A run that ended in failure projects failed, and that state stays until the next run for the Chat starts. A Chat whose record holds no such run projects idle, including a record written before the projection existed.

The projection names one `active_run`: the waiting run when one waits, otherwise the working run with the earliest sequence, and none when the state is failed or idle. None does not mean idle, because a failed state also names no active run.

An old `ChatView` with no `status` reads as idle, and with no `active_run` reads as null. Replaying the record builds the same projection from the run and approval events: a `RunStarted` with no end in the record reads as working, and several waiting runs name the one with the earliest sequence.

A client reads the state from a snapshot and follows the live events from the snapshot's cursor. When the client's cursor reaches the snapshot's, the state it holds equals the state the projection gives.

## Secrets

The Vault and the keys of Providers are encrypted on disk. The key that decrypts them lives in the operating system's key store, or in a file that only the Space's account can read where no key store exists.

## Who can read the record

A Space reads its record in clear. The operator of a Space can read the record and what passes through it from a Local Computer.
