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

An event has a kind, a version, an author, and data. A kind belongs to one Plugin and declares the schema of its data, and Neha rejects data that does not match it.

A reader keeps every event. A kind it does not know that decides who can read or act makes it stop and ask for an upgrade.

## Approval events

The record holds these kinds for approvals and rules:

| Kind | What it records |
| --- | --- |
| `neha.run.reply` | A step's reply: its content, calls, and usage |
| `neha.approval.requested` | A Computer action waiting for the acting User's decision |
| `neha.approval.decided` | The acting User's Allow or Deny for one approval |
| `neha.rule.added` | A lasting Ask or Allow rule |
| `neha.rule.removed` | A lasting rule removed |

Each is an Authority event, so a reader that does not know one stops and asks for an upgrade.

## Projections

A projection is built from the record and can be built again from it. Building a projection runs no tool and sends no message.

## Secrets

The Vault and the keys of Providers are encrypted on disk. The key that decrypts them lives in the operating system's key store, or in a file that only the Space's account can read where no key store exists.

## Who can read the record

A Space reads its record in clear. The operator of a Space can read the record and what passes through it from a Local Computer.
