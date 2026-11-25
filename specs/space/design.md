# Space

[product.md](product.md) states what a Space is for its Users. This page states where a Space keeps its record, how the record grows, and who can read it.

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

A Space keeps its record in one SQLite database on the Space's machine. A File lives beside it in a content-addressed store.

Storage is one seam. The code that reads and writes the record does not name SQLite, and another database can replace it without changing that code.

## Events

An event has a kind, a version, an author, and data. A kind belongs to one Plugin and declares the schema of its data, and Neha rejects data that does not match it.

A reader keeps every event. A kind it does not know that decides who can read or act makes it stop and ask for an upgrade.

## Projections

A projection is built from the record and can be built again from it. Building a projection runs no tool and sends no message.

## Secrets

The Vault and the keys of Providers are encrypted on disk. The key that decrypts them lives in the operating system's key store, or in a file that only the Space's account can read where no key store exists.

## Who can read the record

A Space reads its record in clear. The operator of a Space can read the record and what passes through it from a real Computer.
