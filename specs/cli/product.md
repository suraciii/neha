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

`neha bot update <bot>` changes a Bot's file access. Only the Bot's owner can change it, and the next run uses the new access.

`--full-access on` gives it the daemon's own file rights and removes the shell fence. `--full-access off` restores the Bot's Workspace fence.

`--grant <path>` grants write access to an extra folder; repeat it for more folders. The folder MUST exist on the Local Computer.

`--clear-grants` removes them all and cannot be combined with `--grant`. `neha bot show` prints what is in effect.

`neha chat send --cwd <path>` starts that run's actions in the named directory on a Local Computer. Without it they start in the Bot's Workspace. The directory is recorded with the run and shown on an approval card.

A Hosted Computer runs only in the Bot's Workspace, and a run that names another directory fails. A path that is not absolute is refused.

## Approvals

`neha approval list` shows the approvals that wait for the User: their ids, runs, Bots, tools, reasons, and summaries. A reason is the first use of a Local Computer, a lasting rule, or a folder the owner has not granted.

`neha approval show <id>` prints the action's step, call index, action, Chat, whether the form was ambiguous, the asking rule when the approval names one, and full arguments.

`neha approval allow <id>` or `neha approval deny <id>` answers one; `neha approval allow <id> --always` also makes a lasting Allow rule when the action can make one. When the action cannot make a lasting rule, `--always` is refused.

`neha rule list`, `neha rule add`, and `neha rule remove` manage lasting Ask and Allow rules. A rule names a tool; a shell rule can name how the command starts, and a file tool with a path, such as `read_file`, `write_file`, or `list_files`, can name a path prefix.

At a terminal, `neha chat send` shows a waiting approval with whether the action can be undone and, when the card offers it, for a rule, what Always allow covers.

An ambiguous form gives the reason `a form no rule can judge`, not a rule the User set. When the action cannot make a lasting rule, the card shows no Always allow and no scope.

It offers Allow, Always allow, Deny, or leaving it waiting; without a terminal it prints the run and approval ids and exits, for `neha approval allow` to answer.

## Gaps

- The command surface is not yet specified; it follows the topics as their product contracts settle.
