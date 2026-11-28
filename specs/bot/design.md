# Bot

[product.md](product.md) states what a Bot does for a User. This page states where a Bot's work runs, what a Bot can do, and what holds its state.

## Authority

Owned by bot. A Bot's work depends on [Computer](../computer/product.md) for where its actions execute, on [CLI](../cli/product.md) for how its loop reaches the Space, and on [Identity](../identity/product.md) for the credential its loop uses.

## Laws

1. A Bot's loop acts as the Bot and holds no record.
2. Every Computer action a Bot takes runs on its Computer.
3. What a Bot has seen is reconstructible from the record.
4. On its Computer, a Bot does only what Neha can enforce there.
5. A run stops between two of its actions and never inside one.

## Where work runs

The built-in loop runs with the Space. It reaches the Space as the Bot does, through the command surface, with the credential the Space gave the Bot.

The built-in loop sends a Bot's actions to the Computer: commands, file reads and writes, and screen actions. The Model, the Bot's memory, and its conversation live in the Space.

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

## Waiting and resuming

A loop stops a run when the Bot's Computer goes offline and resumes it when the Computer is back. A Message sent to an offline Computer waits in the record.

A run stops between two actions when it waits for an Approval or an answer, when its Computer goes offline, or when a User takes control of its screen. It resumes after the last recorded action, and a recorded action never runs again.

Resetting a Hosted Computer runs only when its Bot has no queued, running, or waiting run.

A run records the access it starts with, so a resumed run keeps the access it began with even after the owner changes the Bot's access.

A run recorded before the Space kept that snapshot resumes with conservative granted access: no full access and no grants.

A run whose record names a working directory the Space never marked writable does not resume; it fails and asks the User to send the message again, and that directory is not made a write root.

A failed run does not resume; the Bot says so as a Message. A run never resumes a Model stream; [Model](../model/design.md) covers that.

## Approvals

Before a Computer action, the loop asks whether to proceed. A File action asks like any other. The first use of a Local Computer is never covered by a lasting rule.

The Space allows by default. It waits when the action is the first one on a Local Computer for the Bot and acting User on that machine, or when a lasting Ask rule matches without a more specific Allow rule.

A `write_file` that would write outside the Bot's write range also waits for the owner. [Computer](../computer/design.md#isolation) states that range, and the run's own working directory is part of it.

Only a `write_file` of a Bot with granted folders raises that approval, and only when the Space can locate its target: an absolute path, or a relative one resolved against the run's own working directory. Full access raises none.

Full access is turned on by the owner's answer at the consent version the Space states. A request without that version is refused, and nothing is recorded.

A shell that writes outside the range is refused by the Computer's fence instead of being asked about.

An Always allow on that approval grants the action's folder for later runs. The first action of a Local Computer can ask with that reason and shows no Always allow. That grant needs the Bot's Computer to serve granted folders; a Computer that serves none, or that has not answered yet, refuses the answer and records no decision.

A card whose folder no grant can store — a `..` climb, a relative form, or a NUL — also shows no Always allow, and the Space refuses `--always` for it; the action itself still runs once when allowed.

An Ask or Allow rule names one Computer tool: `shell`, `read_file`, `write_file`, or `list_files`. A shell rule has a command prefix and a file-tool rule a path prefix, and a command prefix matches a command when its text, after leading whitespace, starts with the prefix.

`RuleAdd` trims and normalizes a path prefix once and stores it; matching does not trim the stored prefix again. A raw action path is trimmed once, then compared after collapsing repeated `/` and removing `.` components, while keeping the absolute or relative form; `./logs/x`, `logs//x`, and `logs/./x` match a `logs` rule.

`.` is the Workspace root: it matches `./a.txt` but no absolute path. A `..` component is never removed and still follows the fail-closed rule. A path made only of whitespace is ambiguous when the tool has a rule.

A directory name that ends with a space is a scope of its own: `logs` and `logs ` differ.

When an Ask rule and an Allow rule both match, the longer prefix wins, and a rule without a prefix is the shortest. A tie goes to the Ask rule.

A shell command with `;`, `&`, `|`, `$`, a backtick, a parenthesis, `<`, `>`, or a line break is a form a prefix cannot judge. A file path with a `..` component, or an absolute/relative form unlike a rule prefix, is another.

A form a prefix cannot judge asks when the Bot and User have any rule for that tool. With no rule for that tool, the action runs. Such an approval names no rule: its `rule` field is empty, its `ambiguous` field is true, and its Always allow removes no rule.

An Always allow writes an Allow rule from the action itself: the command's first word when the action has a command, or the path's parent directory when it has a path. A relative path at the Workspace root writes `.`.

Only an action with neither a command nor a path writes a tool-only Allow rule.

When an action cannot make a lasting rule, its approval offers no Always allow: a path with a `..` or all-whitespace component, a shell command with no first word, or a file action with no usable path.

An ambiguous form with a derivable scope still offers Always allow, and that answer removes no rule. The Space refuses a lasting answer when no rule can be derived.

The new Allow rule covers the Ask rule when the Ask rule's command prefix starts with the Allow rule's prefix, or when the Ask rule's path equals the Allow rule's path or sits below it. A tool-only Ask rule is not covered by a prefixed Allow rule.

Neha removes the Ask rule the approval named when the new Allow rule covers it. An approval with an empty `rule` removes none, and an uncovered named rule stays.

The approval records the Computer and the call's index. An Allow resumes the run from the record: the step's reply and finished actions replay, the waiting action keeps its recorded id, and no action runs twice. The resumed step binds the answer to that call by index.

A Deny fails the run, and the Bot says why.

A run that waits for an approval keeps no place in the Bot's queue, so a later request for the same Bot runs. A Space restart keeps an undecided approval waiting, resumes an allowed one, and fails a denied one.

## Instructions and data

An instruction to the Bot carries the authority of the User who gave it.

Everything else a Bot reads is data: tool results, files, web pages, messages, and what the Bot noticed itself. Data enters the context with a label that names where it came from.

A message from another surface, a bridge, or another agent carries no authority, whoever it claims to come from.

A skill that the Bot's owner enabled is an instruction. A skill adds no authority, and a script it carries runs only as one of the Bot's actions. A skill the Bot wrote itself becomes an instruction only when its owner approves it.

## Memory and context

A Bot's memory is a set of entries in the record. An entry has an id, its Bot, its kind, its scope, its author, its source, its text, its creation, and whether it is superseded. Its source names the Message, run, or command that wrote it.

An entry has kind `rule` or `note`. A `rule` is a lasting instruction, and only a User's words start one. A `note` is data the Bot noticed and wrote for itself; the Bot extracts no `note` in the background.

A `rule` entry's author is the User whose words gave it, and a `note` entry's author is the Bot. An entry applies only in its scope: an `owner` entry in every Chat of the Bot's owner, a `user` entry in the Chat where it was written, and a `group` entry in the Group where it was written.

The Bot's job is the earliest `rule` among the entries in `owner` scope that still apply. A superseded entry is not a candidate, and a Bot whose owner holds no such rule gets no job section.

The same text written again in the same Bot, kind, scope, and acting User changes nothing, so a replay adds no entry. Two Users who write the same text keep one entry each.

An entry that a write replaces is superseded: it applies to nothing and enters no prompt. A `remember` MAY replace several entries, and its replacement and write commit together.

The built-in loop offers `remember`, `forget`, and `list_memories`. `remember` writes one entry from `kind`, `scope`, and `text`, and MAY name one or more entries it replaces. `forget` removes the entries an id or a text names, and `list_memories` reads the entries that apply to the request.

`forget` by text matches only an entry the caller reads and MAY remove; the owner removes another User's entry by the id its metadata names.

The targets of `remember` are ids of entries the acting User can remove, and a call whose target does not exist or cannot be removed changes nothing.

A memory write is a record event and needs no approval. It records `neha.memory.added`, a removal records `neha.memory.removed`, and [Space](../space/design.md#memory-events) owns those kinds. A memory tool is a record operation and not an Action: it never reaches the Computer, no unknown-action recovery applies to it, and no approval or Ask or Allow rule covers it.

The Bot merges or removes its own `note` entries. A Bot never removes a `rule` on its own. The owner removes any entry, and a User removes the entries that User's words gave and the `note` entries the Bot wrote while acting for that User.

The byte limit of each kind, the refusal of a write that passes it, and the all-or-nothing write follow [product](product.md#memory).

A `remember` result names the entry's id, kind, and scope, and the capacity left for that kind. A `forget` result names the entries it removed.

The system prompt is rebuilt for every request from the entries that apply to the acting User and the conversation. The same record and request give the same prompt.

The prompt states the Bot's job first when the Bot has one, then the other `rule` entries, then the `note` entries under a label that marks them as data, then the skills, then the tool instructions. A Chat where the owner's entries do not apply, and a Group, get no job section.

The Model sees a projection of the record, and the record keeps every original Message. The projection prunes and compacts, and neither writes a Message.

Pruning is a projection step separate from compaction. It treats a tool result as one unit and removes the oldest results from the context first, but the three most recent always stay.

A result beyond the budget becomes a placeholder that names the tool call's id; the body stays in the record, and no File is created for it. A conversation that fits after pruning gets no summary.

Neha compacts a conversation on one path of its own and does not use a Provider's native compaction. A conversation compacts when its estimate reaches its threshold.

When `window <= max_output + 65536`, the window holds no headroom and the threshold is `window − max_output`, at least 0. A window that cannot hold its own maximum output leaves no room to compact.

Otherwise, the threshold is the smaller of 0.8 × the Model's context window and the window minus its maximum output minus 65536 tokens. A Model MAY carry its own threshold.

The estimate starts at the usage the Provider last reported, or at 0 when it reported none. It counts the projection the request will send, so text that pruning or compaction leaves out does not count.

A Provider error that says the input is too long compacts at most once and retries the request at most once. A conversation with no span to compact is not compacted. A retry that reports the same error ends the run with that error and makes no further attempt.

One silent turn comes before a compaction and writes what the Bot has not written down as `note` entries. The turn uses a private copy of the conversation, writes only `note` entries, posts nothing in the conversation, and is skipped when it fails. A compaction makes at most one such turn.

The cut falls on the oldest balanced span that keeps the most recent 16% of the original text raw, and every tool call stays paired with its result. A conversation with no such cut, or one whose Bot has not replied since the previous summary, does not compact.

The compaction state belongs to one Chat or Group and one Bot. The summary is one separate request to the same Model, with a handoff prompt that adds no instruction to the summary and an output cap of 8192 tokens.

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
