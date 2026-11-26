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

## Running a Space

`neha serve` starts a Space on the machine and prints its address and an invitation link. The first User opens the link in an app or a browser to become the Admin.

## Computers

A Local Computer runs the CLI. `neha computer pair` joins the Local Computer on the machine where Neha runs.

It finds the startup ticket the Space wrote under the machine's Neha configuration directory, or asks the Space for one as the signed-in User; either way it pairs the Space the ticket names. `--ticket-file` names another file. `neha computer run` starts the machine again after a stop.

`neha computer list` shows the Computers the User owns, Local and Hosted. `neha bot create --computer` takes one of these:

| Value | What happens |
| --- | --- |
| `local` | The owner's Local Computer; with several, the earliest one recorded |
| `hosted` | A new Hosted Computer, one for the Bot |
| A Local Computer id | The Computer with that id |

Without the option, a new Bot starts on a Hosted Computer. `neha bot move` moves a Bot to another Local Computer.

## Approvals

`neha approval list` shows the approvals that wait for the User: their ids, runs, Bots, tools, reasons, and summaries. `neha approval show <id>` prints the action's step, call index, action, Chat, asking rule, and full arguments.

`neha approval allow <id>` or `neha approval deny <id>` answers one; `neha approval allow <id> --always` also makes a lasting Allow rule.

`neha rule list`, `neha rule add`, and `neha rule remove` manage lasting Ask and Allow rules. A rule names a tool; a shell rule can name how the command starts, and a file tool with a path can name a path prefix.

At a terminal, `neha chat send` shows a waiting approval with whether the action can be undone and, for a rule, what Always allow covers. It offers Allow, Always allow, Deny, or leaving it waiting; without a terminal it prints the run and approval ids and exits, for `neha approval allow` to answer.

## Gaps

- The command surface is not yet specified; it follows the topics as their product contracts settle.
