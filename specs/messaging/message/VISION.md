# Message

## Why

A Bot finishes a deck and posts it. The user opens it, replies underneath with one change, and a second Bot reads the same Message as data — the file, the version, the request — without parsing prose.

Everything exchanged in Neha — words, a file, a result, a request — is a Message. It has to work for a user and for a program, keep its author, and survive being carried to another surface.

## What

A Message is a unit of a Chat's or a Group's stream, written by a User or a Bot.

Its content is text, and it can carry a File or a Card; any Message can carry a Thread.

A Message records its author, its time, and the conversation it belongs to. A Bot's Message also records the User whose authority it acted under.

A File attached to a Message lives in the Space, whatever Computer produced it.

## Why this way

One kind of unit for everything exchanged lets the stream, Threads, and notifications treat a sentence, a deck, and an approval alike.

A Card keeps its data inside its text, checked when written, so a program reads fields and a user reads a card from the same Message.

A Thread is single-level under its Message, so it carries to any surface with one level and keeps the root it answers.

The author is recorded from the verified identity, so a Bot speaks as itself and a User as themselves, and a Bot's Message shows whose authority it used.

## Principles

- **One kind of unit.** Whatever is exchanged is a Message, and every consumer handles every Message alike.
- **Readable by a user and a program.** A Card's data is checked when written and exposed as data when read; its text is what users see.
- **Authorship is verified.** The author and the authority behind it come from the verified identity, and the writer's own claim never sets either.
- **Edits keep history; deletion leaves a place.** An edit is marked, a deletion hides the content and keeps the Message's place, and both carry to every surface.
- **Threads are single-level.** A reply points at its root; what it answers can be noted; a Thread has no Thread inside it.
- **A File lives in the Space.** A File attached to a Message is held by the Space, whatever Computer produced it, so the Message reads the same from any client.

## What this enables

- Users and Bots read the same record.
- A Task is a Message with state, so filters, notifications, and Threads already work on it.
- A Bot's result lands as a Message wherever the work was given.
- Carrying a Message to another surface keeps everything a program needs.

## User stories

- [S1 · Tell the Bot, and it gets done](../../USER-STORIES.md#s1--tell-the-bot-and-it-gets-done)
- [S3 · A team in the open](../../USER-STORIES.md#s3--a-team-in-the-open)
