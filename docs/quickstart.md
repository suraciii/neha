# Quick start: chat with a Bot

This guide starts a Space on one machine, joins it as the Admin, connects an OpenAI-compatible Provider, and chats with a Bot. The result is one Chat where a Bot answers a User and can work in its own Computer workspace with tools.

Everything runs on the local machine. The Space listens on loopback and stores its record in `NEHA_HOME`.

File bytes stay in `NEHA_HOME` by default too. [Storage](storage.md) shows how to keep them in an S3-compatible bucket instead.

## Requirements

- Rust stable, version 1.94 or newer.
- `just` for the verification recipes.
- Node 24 and pnpm 11 for the browser client.
- A Provider that speaks OpenAI Chat Completions, or a compatible local server.

Build the CLI and the browser client:

```bash
cargo build --locked --workspace
pnpm --dir ui install --frozen-lockfile
pnpm --dir ui build
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
Open in a browser: http://neha-<hash>.localhost:7337/invite#<secret>
Or join from the CLI: neha join 'http://127.0.0.1:7337/invite#<secret>' --name <your name>
```

The browser hostname is stable for this Space and keeps its cookie off shared loopback addresses. Use the `Open in a browser` line the CLI prints.

The invitation is valid once and only while this Space runs. Keep the terminal open.

## Join as the Admin in the browser

Open the `Open in a browser` invitation link. The client asks for a display name and a password of at least eight bytes.

The first User is the Admin. The browser keeps the session in an HttpOnly cookie; no token appears in the page or its URL.

The browser client is served from `ui/dist`. `neha serve` finds it automatically, or takes `--ui <directory>` or `NEHA_UI_DIR`.

The sign-in card has a language switch for English and Simplified Chinese. The signed-in client keeps the switch in the first-run setup header, the Chat list footer, and the Chat header; the choice persists in the browser and does not change the record.

## Join as the Admin from the CLI

In a second terminal, join with the same invitation link:

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

## A Bot does one thing at a time

A Bot works on one request at a time. A request that arrives while the Bot works waits in a queue.

The browser client shows the waiting request and how many requests are ahead of it. The run in progress holds the first place in the queue, so the CLI prints `queued (position 2)` for the request behind it. If the CLI observes the run in progress, it prints `running`; it prints the Bot's reply when the run ends.

Send a request that keeps the Bot busy, such as a shell command that waits:

```bash
target/debug/neha chat send "Run 'sleep 5' in the shell, then tell me the exit code."
```

Wait until the first terminal prints `running`, then send a second request from another terminal while the first command runs:

```bash
target/debug/neha chat send "Then list the open questions."
```

If the second command prints `queued (position 1)`, the first run had already ended; send the request sooner to see it wait at position 2.

The second command prints `queued (position 2)` and later the Bot's reply. Its last line names the run:

```text
  run <run-id> (succeeded)
```

Read that run from the record:

```bash
target/debug/neha run show <run-id>
```

`neha run show` prints `position N` while the run waits in the queue, and no position once it starts.

## Send a file

A Message can carry one file of up to 32 MiB. Send one and ask the Bot to work on it:

```bash
printf 'remember the milk\n' > notes.txt
target/debug/neha chat send --file notes.txt "Read the attached file and tell me what it says."
```

[Send and receive Files](files.md) shows how to list and download Files, and how the Bot takes a file into its Workspace and delivers one back.

## Restart and sign in again

Stop the Space with Ctrl-C. Start it again with the same `NEHA_HOME`:

```bash
target/debug/neha serve
```

The record, the Provider, the Bot, and the Messages survive. A new Space run prints no invitation. Sign in with the saved account:

```bash
target/debug/neha login --name Ada
```

When the Space has a new address, or the saved credential is gone, pass `--server` with the API address the Space prints. Then read the old Chat again; it contains every recorded Message.

## What this milestone does not do

- The managed Computer is an in-process workspace and is not isolated. Its shell and file tools execute real commands and real file operations with the Space user's permissions. Do not run a Space exposed to other machines or with prompts you do not trust.
- A Bot has no approval gate yet. It runs an action as soon as the Model asks for it.
- One human joins a Space. Group Chats, Boards, approvals, notifications, and search arrive in later milestones.
- The browser client covers joining, first-run setup, and one-to-one Chat. It does not show Groups, Tasks, approvals, search, or notifications.
- The browser client does not stream a reply. It shows a working state until the Run finishes.
