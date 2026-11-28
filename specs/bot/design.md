# Bot

[product.md](product.md) states what a Bot does for a User. This page states where a Bot's work runs, what a Bot can do, and what holds its state.

## Authority

Owned by bot. A Bot's work depends on [Computer](../computer/product.md) for where its actions execute, on [CLI](../cli/product.md) for how its loop reaches the Space, and on [Identity](../identity/product.md) for the credential its loop uses.

## Laws

1. A Bot's loop acts as the Bot and holds no record.
2. Every action a Bot takes runs on its Computer or through an MCP server; a memory or `plan` tool is a record operation.
3. What a Bot has seen is reconstructible from the record.
4. A Bot does only what Neha can enforce on its Computer, at an MCP server, or in the record.
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

## Steps and actions

A step holds one Model call and the actions that call requests. A completed step never runs again; the record gives its result back to the context. A step interrupted before it completes runs again from its start.

The resumed step replays the results of the actions it already recorded, then continues from the first action it did not record.

An action is recorded before it runs and after it returns. A started action never runs twice.

An action whose result never returns is unknown. The loop checks its effect on the Computer, or asks the User, and does not run that started action again.

## Plan

The record holds a Plan as `neha.run.planned` events. Each event names the run, the step, and the call, and holds the whole `entries` list, where each entry is `{title, state}`.

Every write replaces the whole list, so the last write wins and entries have no identity. An empty list clears the Plan.

The projection keeps the newest event of one Run. A plan write answers one unrefused call of the run's recorded reply with the list that call asked for, and a finished run takes no write. A Plan is not a Message, and a compaction removes no Plan: it is the current state of the Run, not history.

The event is a Presentation event: a reader that does not know it skips it. A Bot writes it acting for the User it acts for, as it writes a memory answer.

The built-in loop offers a `plan` tool that takes `{ entries: [{ title, state }] }`. A `plan` call is a record operation, not an Action: it runs no Computer action, asks no approval, and no Ask or Allow rule covers it.

Each entry names only `title` and `state`, and the states are the three a Plan entry defines. A `plan` call with another state or field is refused and records no Plan.

The refusal is the tool result the Model reads, and a replay returns it.

The bounds of a Plan and the no-change rule for a write that passes one follow [product](product.md#plan).

The `plan` tool's description tells the Model to update an entry when it finishes, and that not every action needs an entry.

Before each ordinary run step request, the current Plan enters the system prompt, rebuilt from the record. The prompt has a stable part and a per-request part; the Plan is the first line of the per-request part. A later capability adds per-step lines after it.

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

An Ask or Allow rule names one tool. On a Computer it names `shell`, `read_file`, `write_file`, or `list_files`, with a command prefix for a shell and a path prefix for a file tool. A command prefix matches a command when its text, after leading whitespace, starts with the prefix.

A rule for `run_skill_script` names a skill and a script, and a rule for a server's tool names the server and the tool; neither carries a prefix.

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

An instruction to the Bot carries the authority of the User who gave it.

Everything else a Bot reads is data: tool results, a server's tool descriptions, files, web pages, messages, and what the Bot noticed itself. Data enters the context with a label that names where it came from.

A message from another surface, a bridge, or another agent carries no authority, whoever it claims to come from.

A skill that the Bot's owner enabled is an instruction. A skill adds no authority, and a script it carries runs only as one of the Bot's actions. A skill the Bot wrote itself becomes an instruction only when its owner approves it.

A skill's script runs as the `run_skill_script` action, whose approval names the skill and the script. Its first run for the Bot and the acting User waits for an Allow, and an Always allow covers that script alone.

## Memory and context

A Bot's memory is a set of entries in the record. An entry has an id, its Bot, its kind, its scope, its author, its source, its text, its creation, and whether it is superseded. Its source names the Message, run, or command that wrote it.

An entry has kind `rule` or `note`. A `rule` is a lasting instruction, and only a User's words start one. A `note` is data the Bot noticed and wrote for itself; the Bot extracts no `note` in the background.

A `rule` entry's author is the User whose words gave it, and a `note` entry's author is the Bot. An entry applies only in its scope: an `owner` entry in every Chat of the Bot's owner, a `user` entry in the Chat where it was written, and a `group` entry in the Group where it was written.

The Bot's job is the earliest `rule` among the entries in `owner` scope that still apply. A superseded entry is not a candidate, and a Bot whose owner holds no such rule gets no job section.

The same text written again in the same Bot, kind, scope, and acting User changes nothing, so a replay adds no entry. Two Users who write the same text keep one entry each.

An entry that a write replaces is superseded: it applies to nothing and enters no prompt. A `remember` MAY replace several entries, and its replacement and write commit together.

The built-in loop offers `remember`, `forget`, and `list_memories`. `remember` writes one entry from `kind`, `scope`, and `text`, and MAY name one or more entries it replaces. `forget` removes the entries an id or a text names, and `list_memories` reads the entries that apply to the request.

`forget` by text matches only an entry the caller reads and MAY remove; the owner removes another User's entry by the id its metadata names.

`remember` names its targets by id, and each target is an entry of the same Bot that still applies and that the acting User can remove. A call with a target that is absent, superseded, or not removable changes no memory entry.

A memory write is a record event and needs no approval. It records `neha.memory.added`, a removal records `neha.memory.removed`, and [Space](../space/design.md#memory-events) owns those kinds. A memory read records no entry change; its answer is still a record event.

A memory tool is a record operation and not an Action: it never reaches the Computer, no unknown-action recovery applies to it, and no approval or Ask or Allow rule covers it.

A memory tool call's answer is a record event: it is appended with the entry changes the call makes, or alone when the call changes no memory entry. It covers a no-op, a refusal, and an error alike. A replay returns the recorded answer and does not run the call again; [Space](../space/design.md#memory-events) owns its kind `neha.memory.answered`.

For one Bot, a memory read, a tool write, and a management removal form one serial order: each is planned and recorded without another of them in flight, and a second removal of one id sees the first one committed. A write and its answer commit together or not at all.

The Bot merges or removes its own `note` entries. A Bot never removes a `rule` on its own. The owner removes any entry, and a User removes the entries that User's words gave and the `note` entries the Bot wrote while acting for that User.

The byte limit of each kind, the refusal of a write that passes it, and the all-or-nothing write follow [product](product.md#memory).

A `remember` result names the entry's id, kind, and scope, and the capacity left for that kind. A `forget` result names the entries it removed.

The system prompt is rebuilt for every request from the entries that apply to the acting User and the conversation. The same record and request give the same prompt.

The prompt states the Bot's job first when it has one, then the other `rule` entries, then the `note` entries under a label that marks them as data, then the skills, then the tool instructions, then the current Plan. A Chat where the owner's entries do not apply, and a Group, get no job section.

Before each ordinary run step request, the loop prunes a copy of the request, including its current and replayed tool results. Compaction changes only the projection the Model sees, and the record keeps every original Message. The projection prunes and compacts, and neither writes a Message.

Pruning is a projection step separate from compaction. It treats a tool result as one unit and removes the oldest results from the context first, but the three most recent always stay.

A result beyond the budget becomes a placeholder that names the tool call's id; the body stays in the record, and no File is created for it.

The budget is the input budget of the Model, and the threshold is the estimate that triggers a summary. After pruning, the run checks the threshold; if the post-pruning estimate is below the threshold, the run skips threshold-triggered compaction.

Neha compacts a conversation on one path of its own and does not use a Provider's native compaction. A conversation compacts when its estimate reaches its threshold.

When `window <= max_output + 65536`, the window holds no headroom and the threshold is `window − max_output`, at least 0. A window that cannot hold its own maximum output leaves no room to compact.

Otherwise, the threshold is the smaller of 0.8 × the Model's context window and the window minus its maximum output minus 65536 tokens. A Model MAY carry its own threshold.

The estimate starts at the normalized usage of the last ordinary run step request, or at 0 when none applies, plus one token for every four characters added since. Only uncached input, cache reads, and cache writes count, never output.

When pruning or a summary replaces text that usage covered, the estimate starts over at 0 and counts the whole projection. A flush or a summary request reports usage that never becomes the base.

A Provider error that says the input is too long compacts the conversation only when a cut is eligible, and at most once. It retries the request at most once, and only when the compaction shortened the input; if the retry reports the same error, the run ends with that error and makes no further attempt.

The flush and the summary request never take this recovery path.

One silent turn comes before a compaction when a cut is eligible and writes what the Bot has not written down as `note` entries. The turn uses a private copy of the conversation, writes only `note` entries, posts nothing in the conversation, and is skipped when it fails. A compaction makes at most one such turn.

A `note` the flush writes follows the byte limits in [product](product.md#memory) and the same no-change rule for identical text. A capacity refusal to a flush `note` stays in the private turn, adds no Message to the Chat, and consumes the one flush turn; the compaction continues.

The cut falls on the oldest balanced span that keeps the most recent 16% of the original text raw, and every tool call stays paired with its result.

A conversation with no such cut, or one whose Bot has not replied since the previous summary, has no eligible cut. It skips the flush and the summary, and it does not compact.

Before its first ordinary run step request, a run checks the threshold, and the threshold check and the recovery from a too-long error use the same eligible cut.

A run compacts its starting history before that request when the estimate reaches the threshold. The compaction state belongs to one Chat or Group and one Bot.

The summary is one separate request to the same Model, with a handoff prompt that adds no instruction to the summary and an output cap of 8192 tokens.

The summary becomes the record event `neha.chat.compacted`, whose data [Space](../space/design.md#chat-events) owns. It is no Message in the conversation, and the Model sees it and the original text after the cut.

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
- A Group conversation does not compact yet.
