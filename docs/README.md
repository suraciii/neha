# Documentation

These pages explain how to build, run, and evaluate Neha. They describe the product as it is used; they do not define product contracts.

Contracts live in [../specs/](../specs/). Domain terms live in [../CONTEXT.md](../CONTEXT.md). Engineering practices live in [../eng/](../eng/).

## Guides

- [Building and verification](building.md) — requirements, commands, and gates.
- [Quick start: chat with a Bot](quickstart.md) — run a Space, connect a Provider, and talk to a Bot.
- [Serve a Space on a domain](deploying.md) — put a TLS proxy in front, install the client on a phone, and keep the listener on loopback.
- [Run a Space in containers](container.md) — build and run a Hosted Space with Docker Compose, Caddy, and one worker container per Hosted Computer.
- [Notifications](notifications.md) — enable Web Push on a phone or desktop, mute the Space, and know what the push service can see.
- [Computers](computer.md) — the Space Computer a new Bot starts on, extra Hosted Computers and their sharing, pairing the machine that runs the Space, and each write fence.
- [Let a Bot work on its Computer](tools.md) — the shell and file tools, the Bot's directory inside its Workspace, and their limits.
- [Approvals](approvals.md) — when a Bot asks before an action, how to answer, and lasting Ask and Allow rules.
- [Send and receive Files](files.md) — send a file in a Chat, download a File, and let a Bot take and deliver Files.
- [Storage](storage.md) — where a Space keeps the bytes of its Files, and how to use an S3-compatible bucket.
- [Upgrade a Space](upgrading.md) — back up the Space's home, replace the binary and the client, and read what the schema migration and the version check do.

New guides are added with the capability they document.
