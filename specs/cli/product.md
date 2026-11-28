# CLI

## Authority

Owned by cli. The CLI depends on every topic for its behavior. No topic depends on the CLI for its behavior; every topic is reached through it.

## Laws

1. Whatever a user can do in the GUI, a user or an agent can do from the CLI.
2. A Bot is a CLI user: it reads, posts, replies, schedules, and watches through the CLI, as a user does.
3. The GUI, the CLI, and a Bot all speak to one Space and see the same record.

## Users of the CLI

| User | How it reaches the CLI |
| --- | --- |
| A user at a terminal | Types commands |
| A user in an app | The app issues commands on the user's behalf |
| A Bot | Its loop issues commands as the Bot |

A Bot posting a File into a Group is a Bot posting a Message that carries the File, through the CLI.

A Bot's shell runs the same CLI with a short-lived run token instead of a sign-in. `neha file get <id>` brings a File into the Workspace. `neha chat send --file <path>` sends one to the Chat as the Bot.

## Running a Space

`neha serve` starts a Space on the machine and prints its address and an invitation link. The first User opens the link in an app or a browser to become the Admin.

## Providers

`neha provider connect <vendor>` connects a vendor the catalog knows, with the vendor's address, protocol, and Models.

```bash
NEHA_PROVIDER_KEY=... neha provider connect openai
```

`neha provider add` connects an endpoint and its Models. The key comes from `NEHA_PROVIDER_KEY` or the terminal prompt, never from the command line.

```bash
NEHA_PROVIDER_KEY=... neha provider add --base-url https://api.example.com/v1 \
  --protocol chat-completions --model example-chat
```

`--discover` replaces the named Models with the endpoint's own `/models` answer. A failed discovery records nothing and asks for `--model`.

`--protocol` is `chat-completions` (the default), `openai-responses`, or `anthropic-messages`. `--thinks` marks the Models as able to think, and `--no-tools` marks them as unable to call tools.

`--thinking minimal|low|medium|high|max` sets the Space's default thinking level; without it the default is `medium`. A Model that is not marked `--thinks` ignores the level.

`neha provider refresh <provider>` reads the Models again from the catalog or the endpoint. A Model whose name is unchanged keeps its identity; a removed Model is retired.

`neha provider list` prints each Provider's vendor, protocol, and address. `neha model list` prints each Model's context, output, image, thinking levels, tools, and default state.

`neha model default <model>` sets the Space's default Model, as [Settings](../client/product.md#settings) does in the browser. Only the Admin sets it, and every Bot without its own Model uses it.

## Catalog

`neha catalog list` prints every vendor in the catalog, its protocol, Model count, and whether Neha can connect it. A vendor Neha cannot connect prints `(not connectable)`; an unsupported protocol is one reason.

`neha catalog status` prints whether the built-in snapshot or the cache is in use, its age, and whether it is stale. `neha catalog refresh` fetches the catalog again; a failed fetch keeps the last known good copy.

Only the Admin refreshes the catalog. The other catalog commands are reads.

## Computers

A Local Computer runs the CLI. `neha computer pair` joins the Local Computer on the machine where Neha runs.

It finds the startup ticket the Space wrote under the machine's Neha configuration directory, or asks the Space for one as the signed-in User; either way it pairs the Space the ticket names. `--ticket-file` names another file. `neha computer run` starts the machine again after a stop.

`neha computer list` shows the Computers the User owns, Local and Hosted. `neha computer reset <computer>` clears a Hosted Computer's Workspace: every file and folder inside it is deleted, and the Workspace folder stays.

Only the Computer's owner resets it, and only a Hosted Computer is reset. A Computer with a queued, running, or waiting run refuses a reset until that run ends, and a second reset of an empty Workspace succeeds.

`neha bot create --computer` takes one of these:

| Value | What happens |
| --- | --- |
| `local` | The owner's Local Computer; with several, the earliest one recorded |
| `hosted` | A new Hosted Computer, one for the Bot |
| A Local Computer id | The Computer with that id |

Without the option, a new Bot starts on a Hosted Computer. A machine where a Hosted Computer cannot start fails the command and names `--computer local` as the alternative. `neha bot move` moves a Bot to another Local Computer.

`neha bot access <bot>` shows a Bot's file access: `full access: on` or `full access: off`, then one `grant <path>` line per granted folder, or `grants: none`. `neha bot show` prints the same lines and the Bot's own Model and thinking level when set.

A Hosted Bot has no file access to change: its Workspace is the only place where its shell writes persistent files.

`neha bot create --model <model>` chooses the Bot's Model, and `--thinking minimal|low|medium|high|max` its level. Without them the Bot uses the Space's default. `neha model list` shows the Models and their levels.

`neha bot set <bot> --model <model> --thinking <level>` changes the Bot's Model or level. A Model that cannot call tools refuses a run, and a level the Model does not accept is refused.

`neha bot set <bot> --reset-model` puts the Bot back on the Space's default Model, and `--reset-thinking` puts it back on the Space's default level; the command prints `Space default` for each. A reset names no value: giving `--model` with `--reset-model`, or `--thinking` with `--reset-thinking`, is refused. The browser's [Bot page](../client/product.md#bots) does the same.

`neha bot rename <bot> <name>` changes the Bot's name; the browser's [Bot page](../client/product.md#bots) does the same.

The access commands below change a Local Bot's file access. Only the Bot's owner can change it, and the next run uses the new access. A Hosted Bot refuses a file-access change because its Workspace is its only write range.

`neha bot grant <bot> <path>` adds write access to one folder, and `neha bot revoke <bot> <path>` takes that access away. The folder MUST exist on the Local Computer when the owner grants it; a revoke matches the path `neha bot access` shows, so it also removes a grant whose folder is gone.

A revoke compares the path exactly as the owner spelled it first: a name that ends in a space takes the grant for that name.

Only the Bot's owner can change a Bot's access. A run uses the access recorded when it starts, so a change made during a run applies to the next one.

`neha bot full-access <bot> on` gives the Bot the machine user's own file rights and starts no sandbox for it; `neha bot full-access <bot> off` restores the granted folders fence. The `on` command sends the current consent version with the owner's answer.

`on` prints the risk, which names the configuration directory the Bot can then read, and asks for confirmation at a terminal. Without a terminal it needs `--yes`, and `off` asks for no confirmation.

`neha chat send --cwd <path>` starts that run's actions in the named directory on a Local Computer. Without it they start in the Bot's Workspace. A Hosted Computer runs only in the Bot's own Workspace, and a run that names another directory fails.

The directory is that run's write root, is recorded with the run and shown on an approval card, and grants nothing to a later run. A path that is not absolute is refused.

The directory MUST exist and be a folder. In granted mode a directory inside a hidden root, or one that holds a hidden path, is refused. The shell mounts an external write root read-write only when its tree holds no link or special file and stays inside the Computer's scan limits; the file tools check every write path.

## Memory

The Bot's owner and a User allowed to use the Bot read its memory from the CLI. [Bot](../bot/product.md#memory) owns what a memory entry is, its kinds and scopes, and who reads and clears one.

`neha bot memory <bot>` prints the memory entries the User reads. Each entry prints its id, kind, scope, author, source, size, and content. The owner also sees every entry the owner does not read, with its metadata only.

The command prints each kind's total used size and its limit. With no entry, it says to ask the Bot in a Chat to remember something.

`neha bot memory remove <bot> <entry>` removes the entry with that id when the User can clear it, and refuses one the User cannot clear. The memory commands read and remove entries; a User adds and changes memory in a Chat.

## Approvals

`neha approval list` shows the approvals that wait for the User: their ids, runs, Bots, tools, reasons, and summaries. A reason is the first use of a Local Computer, a lasting rule, or a folder the owner has not granted.

`neha approval show <id>` prints the action's step, call index, action, Chat, whether the form was ambiguous, the asking rule when the approval names one, and full arguments.

`neha approval allow <id>` or `neha approval deny <id>` answers one; `neha approval allow <id> --always` also makes a lasting Allow rule when the action can make one. When the action cannot make a lasting rule, `--always` is refused.

`neha rule list`, `neha rule add`, and `neha rule remove` manage lasting Ask and Allow rules. An Ask or Allow rule names one Computer tool the loop offers — `shell`, `read_file`, `write_file`, or `list_files` — and belongs to a Local Bot. A rule that names a memory tool or an unknown tool is refused, as is a rule on a Hosted Bot.

A shell rule can name how the command starts, and a file tool with a path, such as `read_file`, `write_file`, or `list_files`, can name a path prefix.

At a terminal, `neha chat send` shows a waiting approval with whether the action can be undone and, when the card offers it, for a rule, what Always allow covers.

An ambiguous form gives the reason `a form no rule can judge`, not a rule the User set. When the action cannot make a lasting rule, the card shows no Always allow and no scope.

It offers Allow, Always allow, Deny, or leaving it waiting; without a terminal it prints the run and approval ids and exits, for `neha approval allow` to answer.

## Gaps

- The command surface is not yet specified; it follows the topics as their product contracts settle.
