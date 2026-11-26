# Quick start: chat with a Bot

This guide starts a Space on one machine, joins it as the Admin, connects an OpenAI-compatible Provider, and chats with a Bot. The result is one Chat where a Bot answers a User and can work in its own Computer workspace with tools.

Everything runs on the local machine. The Space listens on loopback and stores its record in `NEHA_HOME`.

## Requirements

- Rust stable, version 1.94 or newer.
- `just` for the verification recipes.
- A Provider that speaks OpenAI Chat Completions, or a compatible local server.

Build the CLI:

```bash
cargo build --locked --workspace
```

## Start a Space

Pick a directory for the Space and start it:

```bash
export NEHA_HOME="$HOME/.local/share/neha/demo"
target/debug/neha serve
```

The CLI prints the address and, for a Space with no Admin, one invitation link:

```text
Neha is listening on http://127.0.0.1:7337
Invite: http://127.0.0.1:7337/invite#<secret>
```

The invitation is valid once and only while this Space runs. Keep the terminal open.

## Join as the Admin

In a second terminal, join with the invitation link:

```bash
target/debug/neha join 'http://127.0.0.1:7337/invite#<secret>' --name Ada
```

The CLI asks for a password of eight bytes or more, then saves a device token in `$XDG_CONFIG_HOME/neha/credential.json` with mode 0600. Later commands use the saved Space URL and token.

Check the session:

```bash
target/debug/neha whoami
```

## Connect a Provider

Add a Provider and its default Model. Put the API key in the environment or let the CLI prompt for it; never put it in the command line:

```bash
NEHA_PROVIDER_KEY=sk-... target/debug/neha provider add \
  --base-url https://api.example.com/v1 \
  --model example-chat
```

The API key is sealed with AES-256-GCM under a master key. The master key comes from `NEHA_MASTER_KEY`, or from `$NEHA_HOME/secrets/master.key`, which the Space creates with mode 0600.

List what the Space offers:

```bash
target/debug/neha provider list
target/debug/neha model list
```

## Create a Bot and chat

A new Bot gets its own Chat and its own Computer:

```bash
target/debug/neha bot create Scout
```

The output names the Bot, its Chat, and its Computer. Show them again, including the Computer's workspace:

```bash
target/debug/neha bot show <bot-id>
```

Send a Message and wait for the Bot's reply, including any steps it took on its Computer. With one Chat, no identifier is needed:

```bash
target/debug/neha chat send "hello"
```

The CLI prints the Bot's reply and the run that produced it. Read the whole Chat or inspect the run's steps and actions:

```bash
target/debug/neha chat read <chat-id>
target/debug/neha run show <run-id>
```

A Bot can also write files, read them back, and run shell commands in its Computer's workspace. [Let a Bot work on its Computer](tools.md) shows how.

## Restart and sign in again

Stop the Space with Ctrl-C. Start it again with the same `NEHA_HOME`:

```bash
target/debug/neha serve
```

The record, the Provider, the Bot, and the Messages survive. A new Space run prints no invitation. Sign in with the saved account:

```bash
target/debug/neha login --name Ada
```

When the Space has a new address, or the saved credential is gone, pass `--server http://127.0.0.1:<port>`. Then read the old Chat again; it contains every recorded Message.

## What this milestone does not do

- The managed Computer is an in-process workspace and is not isolated. Its shell and file tools execute real commands and real file operations on the Space's machine. Do not run a Space exposed to other machines.
- A Bot has no approval gate yet. It runs an action as soon as the Model asks for it.
- One human joins a Space. Group Chats, Files, Boards, approvals, notifications, and search arrive in later milestones.
