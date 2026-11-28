# Quick start: chat with a Bot

This guide starts a Space on one machine, joins it as the Admin, connects an OpenAI-compatible Provider, and chats with a Bot. The result is one Chat where a Bot answers a User and can work in its own directory inside its Computer's Workspace with tools.

Everything runs on the local machine. The Space listens on loopback and stores its record in `NEHA_HOME`. To reach it from a phone or another computer, [serve it on a domain](deploying.md).

File bytes stay in `NEHA_HOME` by default too. [Storage](storage.md) shows how to keep them in an S3-compatible bucket instead.

## Requirements

- Rust, pinned by [`rust-toolchain.toml`](../rust-toolchain.toml); the MSRV is 1.94.
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

Open the `Open in a browser` invitation link. The client asks for a display name and a password of at least eight characters; the Space checks at least eight bytes.

The first User is the Admin. The browser keeps the session in an HttpOnly cookie; no token appears in the page or its URL.

The browser client is served from `ui/dist`. `neha serve` finds it automatically, or takes `--ui <directory>` or `NEHA_UI_DIR`.

The sign-in card has a language switch for English and Simplified Chinese. The signed-in client keeps the switch in the Chat list footer; the first-run setup header carries it at narrow widths, where the sidebar is hidden. The choice persists in the browser and does not change the record.

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

Neha ships the models.dev catalog. Connect a vendor it knows; the vendor's address, protocol, Models, and capabilities come from the catalog:

```bash
target/debug/neha catalog list
NEHA_PROVIDER_KEY=sk-... target/debug/neha provider connect openai
```

`neha catalog status` shows whether the built-in snapshot or a cached catalog is in use. `neha catalog refresh` fetches it again; a failed fetch keeps the last good copy. A vendor Neha cannot connect is listed with `(not connectable)`; an unsupported protocol is one reason, and a templated address or no tool-calling Model is another.

For any other OpenAI-compatible endpoint, add it explicitly. `--discover` asks the endpoint for its Models; without it, name each Model:

```bash
NEHA_PROVIDER_KEY=sk-... target/debug/neha provider add \
  --base-url https://api.example.com/v1 --discover
NEHA_PROVIDER_KEY=sk-... target/debug/neha provider add \
  --base-url https://api.example.com/v1 --model example-chat
```

`--protocol` selects `chat-completions` (the default), `openai-responses`, or `anthropic-messages`. Add `--thinks` when the Model can think, `--no-tools` when it cannot call tools, and `--thinking minimal|low|medium|high|max` to set the Space's default level (default `medium`). `neha provider refresh <provider>` reads the Models again.

The API key is sealed with AES-256-GCM under a master key. The master key comes from `NEHA_MASTER_KEY`, or from `$NEHA_HOME/secrets/master.key`, which the Space creates with mode 0600.

A fetched catalog is cached at `$NEHA_HOME/catalog/catalog.json`; without a cache, the built-in snapshot is used. `config.toml` can point at another catalog and change the background refresh interval; `refresh_hours = 0` disables it:

```toml
[catalog]
url = "https://models.dev/api.json"
refresh_hours = 24
```

List what the Space offers:

```bash
target/debug/neha provider list
target/debug/neha model list
```

## Create a Bot and chat

A new Bot gets its own Chat and starts on your Space Computer. That is the Hosted Computer every User receives, and the Space creates it the first time a Bot needs one.

```bash
target/debug/neha bot create Scout
BOT_ID=$(target/debug/neha bot list | awk '$2 == "Scout" {print $1; exit}')
CHAT_ID=$(target/debug/neha chat list | awk '{print $1; exit}')
```

`neha bot list` prints each Bot's id, name, and Chat; `neha chat list` prints each Chat's id and Bot. The two lines above keep the ids in the shell for the rest of this guide.

A Bot uses the Space's default Model and thinking level. Choose its own with `--model <model-id>` and `--thinking minimal|low|medium|high|max`; `neha bot show` prints them when set. `neha bot set <bot> --model <model-id> --thinking <level>` changes them later.

The output names the Bot, its Chat, and its Computer. Show them again, including the Computer's Workspace root:

```bash
target/debug/neha bot show "$BOT_ID"
```

For a Hosted Computer, `neha bot show` prints the Workspace root, and the Bot works in a directory of its own inside it: `$NEHA_HOME/workspaces/<computer-id>/<bot-id>`.

Send a Message and wait for the Bot's reply, including any steps it took on its Computer. With one Chat, no identifier is needed:

```bash
target/debug/neha chat send "hello"
```

The CLI prints the Bot's reply and the run that produced it. Read the whole Chat:

```bash
target/debug/neha chat read "$CHAT_ID"
```

A Bot can also write files, read them back, and run shell commands in its own directory inside its Computer's Workspace. [Let a Bot work on its Computer](tools.md) shows how.

## A Bot does one thing at a time

A Bot works on one request at a time. A request that arrives while the Bot works waits in a queue.

The browser client shows the waiting request and how many requests are ahead of it. The run in progress holds the first place in the queue, so the CLI prints `queued (position 2)` for the request behind it. If the CLI observes the run in progress, it prints `running`; it prints the Bot's reply when the run ends.

Send a request that keeps the Bot busy, such as a shell command that waits:

```bash
target/debug/neha chat send "Run 'sleep 5' in the shell, then tell me the exit code."
```

Wait until the first terminal prints `running`, then send a second request from another terminal while the first command runs. Keep the run id the reply prints:

```bash
target/debug/neha chat send "Then list the open questions." | tee reply.txt
RUN_ID=$(awk '/^  run / {print $2; exit}' reply.txt)
```

If the second command prints `queued (position 1)`, the first run had already ended; send the request sooner to see it wait at position 2.

The second command prints `queued (position 2)` and later the Bot's reply. Its last line names the run:

```text
  run <run-id> (succeeded)
```

Read that run from the record:

```bash
target/debug/neha run show "$RUN_ID"
```

`neha run show` prints `position N` while the run waits in the queue, and no position once it starts.

## Put a Bot on a Local Computer

A Local Computer is the machine where Neha runs: a Neha process that connects out to the Space and runs each Bot's actions in the Bot's own directory inside that Computer's Workspace on the machine. This milestone pairs the machine that runs the Space, because the Space listens on loopback only.

The Space runs in one terminal. Open another terminal and pair this machine. There is no pairing code: the command gets a pairing ticket from the Space and asks you to confirm.

```bash
target/debug/neha computer pair
```

Answer `y`. It prints `Paired Computer <id> with <url>.`; the Computer belongs to you. Then start the daemon and keep this terminal open:

```bash
target/debug/neha computer run
```

In another terminal, list the Computers and create a Bot on the Local one. `computer list` prints each Computer's id, name, kind, sharing, online state, and workspace; this machine shows `local` and the Space's Hosted Computers show `hosted`. The workspace column is empty for this machine because the Space records no path for a Local Computer, and the daemon's `Workspace root:` line names it:

```bash
target/debug/neha computer list
target/debug/neha bot create Runner --computer local
RUNNER_ID=$(target/debug/neha bot list | awk '$2 == "Runner" {print $1; exit}')
```

Without `--computer`, a new Bot goes on your Space Computer; `--computer local` puts it on your paired Local Computer. `--computer hosted` gives the Bot a new exclusive Hosted Computer of its own, and a Computer id names any Computer you own.

Ask the new Bot to work:

```bash
target/debug/neha chat send --bot "$RUNNER_ID" "pwd; echo hi > a.txt"
```

The first action on this Local Computer asks for approval. At the card, type `a` to allow it once; [Approvals](approvals.md) covers the other answers.

The Bot works in its own directory on the machine, `~/.neha/workspaces/<bot-id>` by default, and `pwd` prints that folder. The machine needs `bubblewrap`: each shell command runs in a sandbox that allows persistent writes only in the Bot's own directory, the folders its owner granted, and the directory a run names with `--cwd`, unless the Bot has full access, and the file tools check their own write paths. Files move through the Bot's shell with `neha file get` and `neha chat send --file`. [Computers](computer.md) covers the fence, pairing, offline behavior, and [moving a Bot](computer.md#moving-a-bot).

## Manage a Bot in the browser

The signed-in client has four entries in its rail: Chats, Needs attention, Bots, and Settings. Open Bots and pick a Bot to open its page.

The page shows the Bot's name, its picture made from the initials of its name, its Computer with its name, kind, sharing, and whether it is online, its Model and thinking level, and its access. Change the name there. Choose another Model or thinking level for the Bot; without one it uses the Space's default, and clearing one puts it back on that default. Move the Bot to another Computer you own from that page.

For a Bot on a Local Computer, the page shows the folders the Bot may write and whether it has full access. Grant a folder there, or turn full access on: the page states the risk and asks you to confirm before anything changes. The page also lists the Bot's Ask and Allow rules, and you can add and remove them there.

Settings shows the Space's Providers and Models, the catalog's state, and every Computer you own with its name, kind, sharing, and whether it is online. Create a Hosted Computer there, choosing its name and whether it is shared or exclusive, and make an exclusive one shared. Set the Space's default Model there.

A Hosted Computer offers a reset in that list; it asks for confirmation first, and it clears every Bot's directory in the Computer's Workspace. A Bot moves on its own page.

The CLI does the same work. Read the ids from the lists, then rename a Bot and set the default Model:

```bash
BOT_ID=$(target/debug/neha bot list | awk '$2 == "Scout" {print $1; exit}')
target/debug/neha bot rename "$BOT_ID" "Scout Two"
MODEL_ID=$(target/debug/neha model list | awk '/  tools/ {print $1; exit}')
target/debug/neha model default "$MODEL_ID"
```

[Approvals](approvals.md) covers the rules and the answers, and [Computers](computer.md#file-access-and-a-runs-directory) covers the folders and full access.

## Send a file

A Message can carry one file of up to 32 MiB. Send one and ask the Bot to work on it. This Space now has two Chats, so name the Bot:

```bash
printf 'remember the milk\n' > notes.txt
target/debug/neha chat send --file notes.txt --bot "$RUNNER_ID" "Read the attached file and tell me what it says."
```

[Send and receive Files](files.md) shows how to list and download Files, and how the Bot takes a file into its own directory and delivers one back.

## Connect a client over the network

A Space that a desktop or phone client reaches over the network sits behind a
trusted TLS proxy. The proxy forwards to the Space's loopback listener and
preserves the public `Host` and `Origin` headers. Start the Space with its
public origin:

```sh
NEHA_PUBLIC_URL=https://space.example.com neha serve
```

The printed invitation uses that public origin. The Space accepts the proxy's
Host and `https` Origin, issues a `Secure` session cookie, and redirects a
loopback browser request to the public origin. Do not expose the loopback
listener directly; terminate TLS at the proxy. A desktop client connects with
the Space URL, or with the invitation link for a Space that has no Admin yet.

For the same origin from a host, `neha serve --domain space.example.com` works
too. [Serve a Space on a domain](deploying.md) covers the proxy, phone
installation, and limits.

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

- The Hosted Computer runs in a `bubblewrap` sandbox on the Space's machine. The sandbox mounts the machine's system directories read-only and the Bot's own directory inside the Computer's Workspace as the only place where files persist; it has its own `/tmp` and process list, and it shares the machine's network. It cannot see the Space's record, its secrets, its File store, or another Computer's Workspace.
- Hosted Computers run only in the Space process in this milestone. Starting, stopping, or deleting one on demand, and worker containers or a Supervisor, are not available.
- A Hosted Computer has no screen, browser, Vault credential, or Take control yet, and it does not ask before an action. [Approvals](approvals.md) covers the gate on a Local Computer.
- An unattended run's approval does not expire yet.
- One human joins a Space. Group Chats, Boards, and search arrive in later milestones.
- The browser client covers joining, first-run setup, and one-to-one Chat with its live state, tool activity, and the Approvals that wait, plus the Bot page, Settings, and the Computers list. It does not show Groups, Tasks, or search. Notifications are available in an installed browser client on a public HTTPS origin; see [Notifications](notifications.md).
- The Bot's page shows a Bot's picture and who can use it. Changing the picture, and sharing a Bot with other Users, come later.
- A Bot on a Hosted Computer does not ask before an action yet, so its page offers no Ask or Allow rule to add or remove.
- The browser client does not stream a reply. It shows a working state while the Run works, and one activity card starts collapsed with the state, the step, and the tools in its title row. An action appears when it starts and updates with its result, and a failed Run expands the card to its reason.
