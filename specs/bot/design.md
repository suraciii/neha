# Bot

[product.md](product.md) states what a Bot does for a User. This page states where a Bot's work runs, what a Bot can do, and what holds its state.

## Authority

Owned by bot. A Bot's work depends on [Computer](../computer/product.md) for where its actions execute, on [CLI](../cli/product.md) for how its loop reaches the Space, and on [Identity](../identity/product.md) for the credential its loop uses.

## Laws

1. A Bot's loop acts as the Bot and holds no record.
2. Every action a Bot takes runs on its Computer or through an MCP server.
3. What a Bot has seen is reconstructible from the record.
4. A Bot does only what Neha can enforce on its Computer or at an MCP server.
5. A run stops between two of its actions and never inside one.

## Where work runs

The built-in loop runs with the Space. It reaches the Space as the Bot does, through the command surface, with the credential the Space gave the Bot.

The built-in loop sends a Bot's actions to the Computer: commands, file reads and writes, and screen actions. The Model, the Bot's memory, and its conversation live in the Space.

A tool of an MCP server is an action the Space runs: it calls the server and records the answer. [Plugin](../plugin/design.md#tools) states how the Model sees such a tool.

A Bot moves Files with its CLI, not with a tool: its shell can run `neha file get` and `neha chat send --file` with the run token in its environment. The system prompt carries this built-in skill and tells the Bot never to print the token.

An outside harness is a loop that runs on the Bot's Computer. Its work lands in the record as any Bot's work does.

The Bot's Computer can change, and a move changes only where its actions run.

## State

The record holds a run: its requests, its steps, the actions the Bot took, and what the Bot saw. A demonstration holds what the User did and saw while in control, and never holds a credential.

A summary the Bot writes is a record event. The history a Model sees is a projection of the record, and the record keeps every original Message.

A Bot's memory is a set of entries in the record that belong to the Bot. Each entry records its author, where it came from, and the scope it applies to. The system prompt is rebuilt for every request from the entries that apply to the acting User and the conversation.

## Steps and actions

A step holds one Model call and the actions that call requests. A completed step never runs again; the record gives its result back to the context. A step interrupted before it completes runs again from its start.

The resumed step replays the results of the actions it already recorded, then continues from the first action it did not record.

An action is recorded before it runs and after it returns. A started action never runs twice.

An action whose result never returns is unknown. The loop checks its effect on the Computer, or asks the User, and does not run that started action again.

## Waiting and resuming

A loop stops a run when the Bot's Computer goes offline and resumes it when the Computer is back. A Message sent to an offline Computer waits in the record.

A run stops between two actions when it waits for an Approval or an answer, when its Computer goes offline, or when a User takes control of its screen. It resumes after the last recorded action, and a recorded action never runs again.

Resetting a Hosted Computer runs only when its Bot has no queued, running, or waiting run.

A run records the access it starts with, so a resumed run keeps the access it began with even after the owner changes the Bot's access.

A run recorded before the Space kept that snapshot resumes with conservative granted access: no full access and no grants.

A run whose record names a working directory the Space never marked writable does not resume; it fails and asks the User to send the message again, and that directory is not made a write root.

A failed run does not resume; the Bot says so as a Message. A run never resumes a Model stream; [Model](../model/design.md) covers that.

## Approvals

Before an action, the loop asks whether to proceed. A File action asks like any other. The first use of a Local Computer is never covered by a lasting rule.

The Space allows by default. It waits when the action is the first one on a Local Computer for the Bot and acting User on that machine, or when a lasting Ask rule matches without a more specific Allow rule.

The Space also waits before the first call of a tool an MCP server gives the Bot and the acting User, and before the first run of a skill's script for them. An Always allow there writes a rule for that tool or that script alone.

A `write_file` that would write outside the Bot's write range also waits for the owner. [Computer](../computer/design.md#isolation) states that range, and the run's own working directory is part of it.

Only a `write_file` of a Bot with granted folders raises that approval, and only when the Space can locate its target: an absolute path, or a relative one resolved against the run's own working directory. Full access raises none.

Full access is turned on by the owner's answer at the consent version the Space states. A request without that version is refused, and nothing is recorded.

A shell that writes outside the range is refused by the Computer's fence instead of being asked about.

An Always allow on that approval grants the action's folder for later runs. The first action of a Local Computer can ask with that reason and shows no Always allow. That grant needs the Bot's Computer to serve granted folders; a Computer that serves none, or that has not answered yet, refuses the answer and records no decision.

A card whose folder no grant can store — a `..` climb, a relative form, or a NUL — also shows no Always allow, and the Space refuses `--always` for it; the action itself still runs once when allowed.

A rule names one tool, with a command prefix for a shell, a path prefix for a file tool, or a skill and a script for `run_skill_script`. A command prefix matches a command when its text, after leading whitespace, starts with the prefix. A rule for a server's tool names the server and the tool and carries no prefix.

`RuleAdd` trims and normalizes a path prefix once and stores it; matching does not trim the stored prefix again. A raw action path is trimmed once, then compared after collapsing repeated `/` and removing `.` components, while keeping the absolute or relative form; `./logs/x`, `logs//x`, and `logs/./x` match a `logs` rule.

`.` is the Workspace root: it matches `./a.txt` but no absolute path. A `..` component is never removed and still follows the fail-closed rule. A path made only of whitespace is ambiguous when the tool has a rule.

A directory name that ends with a space is a scope of its own: `logs` and `logs ` differ.

When an Ask rule and an Allow rule both match, the longer prefix wins, and a rule without a prefix is the shortest. A tie goes to the Ask rule.

A shell command with `;`, `&`, `|`, `$`, a backtick, a parenthesis, `<`, `>`, or a line break is a form a prefix cannot judge. A file path with a `..` component, or an absolute/relative form unlike a rule prefix, is another.

A form a prefix cannot judge asks when the Bot and User have any rule for that tool. With no rule for that tool, the action runs. Such an approval names no rule: its `rule` field is empty, its `ambiguous` field is true, and its Always allow removes no rule.

On a Computer, an Always allow writes an Allow rule from the action itself: the command's first word when the action has a command, or the path's parent directory when it has a path. A relative path at the Workspace root writes `.`.

On a Computer, only an action with neither a command nor a path writes a tool-only Allow rule. `run_skill_script` writes a rule for its skill and script, never one for the whole tool or a directory.

When an action on a Computer cannot make a lasting rule, its approval offers no Always allow: a path with a `..` or all-whitespace component, a shell command with no first word, or a file action with no usable path.

An ambiguous form with a derivable scope still offers Always allow, and that answer removes no rule. The Space refuses a lasting answer when no rule can be derived.

The new Allow rule covers the Ask rule when the Ask rule's command prefix starts with the Allow rule's prefix, or when the Ask rule's path equals the Allow rule's path or sits below it. A tool-only Ask rule is not covered by a prefixed Allow rule.

An Allow rule for a server's tool covers a tool-only Ask rule for the same server and tool. An Allow rule for a skill's script covers an Ask rule for the same skill and script.

Neha removes the Ask rule the approval named when the new Allow rule covers it. An approval with an empty `rule` removes none, and an uncovered named rule stays.

The approval records the Computer when the action runs on one, otherwise the MCP server, and the call's index. An Allow resumes the run from the record: the step's reply and finished actions replay, the waiting action keeps its recorded id, and no action runs twice. The resumed step binds the answer to that call by index.

A Deny fails the run, and the Bot says why.

A run that waits for an approval keeps no place in the Bot's queue, so a later request for the same Bot runs. A Space restart keeps an undecided approval waiting, resumes an allowed one, and fails a denied one.

## Instructions and data

A lasting rule is an instruction to the Bot within the authority of the User who gave it.

Everything else a Bot reads is data: tool results, a server's tool descriptions, files, web pages, messages, and what the Bot noticed itself. Data enters the context with a label that names where it came from.

A message from another surface, a bridge, or another agent carries no authority, whoever it claims to come from.

A skill that the Bot's owner enabled is an instruction. A skill adds no authority, and a script it carries runs only as one of the Bot's actions. A skill the Bot wrote itself becomes an instruction only when its owner approves it.

A skill's script runs as the `run_skill_script` action, whose approval names the skill and the script. Its first run for the Bot and the acting User waits for an Allow, and an Always allow covers that script alone.

## Authority for an action

On a Computer, a Bot does what that Computer's owner granted. The User a Bot acts for approves each action that needs an approval.

An approval covers one action of one run, and Neha checks it again when the action runs. It ends when that User leaves the conversation, when the Computer is revoked, or when the run is cancelled.

A lasting rule from a User does not cover an action the Computer's owner requires asking for.

## Runs

A Bot executes one run at a time.

While a Bot executes a run, a later request for that Bot waits in a queue, in the order the Space recorded it.

A Message from the User the Bot acts for that answers the run's request steers the run.

Runs of different Bots run in parallel.

A handoff from one Bot to another carries the authority of the User the first Bot acted for, and never more.

## Gaps

- The Model does not ask for an approval on its own; only a built-in trigger or a lasting Ask rule asks.
- Cancelling an approval and an unattended approval's expiry are not yet available.
- An independent reviewer of approvals is not yet specified.
