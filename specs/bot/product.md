# Bot

## Authority

Owned by bot. A Bot depends on [Computer](../computer/product.md) for where it lives, on [Chat](../messaging/chat/product.md) for its conversation with its owner, and on [Space](../space/product.md) for the Users it can be shared with.

## Laws

1. A Bot has exactly one owner.
2. A Bot's memory is its own and goes where the Bot goes.
3. A new Bot starts on a Hosted Computer. Its owner can create it on the Local Computer where Neha runs instead.

## Creating a Bot

A new Space shows an empty sidebar with one action: create a Bot.

A User creates a Bot with a name and a picture. The Bot starts on its Computer, and its Chat with the owner opens.

The owner gives the Bot its job and its rules in that Chat, and asks it to remember them.

A Bot works once the Space has a Provider.

## The Bot's page

A Bot has a page with its name, picture, Computer, access, Model, and memory. The owner edits its name, picture, Computer, access, and Model; other Users read them. The owner views the memory entries and deletes any of them there; [Client](../client/product.md#memory) owns the page.

| Field | What it holds | Who changes it |
| --- | --- | --- |
| Name | The Bot's name | The owner, on the page or with `neha bot rename` |
| Picture | The Bot's avatar | The owner |
| Computer | The Hosted or Local Computer it lives on | The owner, on the page or with `neha bot move` |
| Access | Who else can use the Bot, the folders it can write, whether it has full access, and its Ask and Allow rules | The owner |
| Model | The Model it thinks with and its thinking level; the Space's default unless the owner chooses | The owner; the Admin sets the Space's default |
| Memory | The entries it keeps, as `rule` and `note`, and the room each kind has left | The Bot writes its `note` entries; a User's words start a `rule`; the owner clears any entry |

The page shows what the Bot can reach on its Computer: the granted folders, full access, and the Ask and Allow rules. [Computer](../computer/product.md#what-a-bot-can-do) owns the folders and full access, and [asking before an action](#asking-before-an-action) owns the rules.

The owner chooses the Bot's Model and thinking level, and clears each back to the Space's default. A clear and a choice for the same field in one change are refused, so the result never depends on their order. [Model](../model/product.md) covers connecting Providers and choosing a Model.

A change to the Bot's Model or its thinking level leaves the Bot able to run: the Space refuses one that leaves a thinking level the Model cannot serve, and names the reset that fixes it.

## Moving a Bot

[Computer](../computer/product.md#assigning-a-computer) covers who can move a Bot and where. Its memory, its Chats, and the Files it delivered stay with it, and it starts in its own Workspace on the new Computer. Work it did not deliver stays on the old Computer.

A move to a Computer that does not serve the access the Bot carries is refused, and the refusal names what to revoke or turn off first. A Bot that moves keeps its grants and its full access, and they mean the same on the new Computer.

## Memory

A Bot keeps what it learns as memory entries of two kinds.

| Kind | What it is | What starts it |
| --- | --- | --- |
| `rule` | A lasting instruction; the Bot's job is the earliest `rule` in `owner` scope that still applies | A User's words, never the Bot on its own |
| `note` | What the Bot noticed, kept as data | The Bot, with its memory tool |

The default scope is the owner's Chats; a Chat entry belongs to that Chat's User, and a Group entry belongs to the Group.

| Scope | Applies in | Who reads it | Who clears it |
| --- | --- | --- | --- |
| Owner | The owner's Chats | The owner | The owner |
| User | One other User's Chat | That User | That User, and the owner |
| Group | The Group where it was learned | Every member of the Group | Its author, and the owner |

When a User asks a Bot to remember something, it keeps the entry at the scope of that conversation. A User can ask what the Bot remembers. The Bot answers from the entries that apply to that request, and it deletes an entry when the User who can clear it asks.

The owner can audit every entry's scope, author, and source without reading a User entry's content. A User sees and clears the entries that are theirs, including the `note` entries the Bot wrote while acting for that User.

The Bot merges or removes its own `note` entries. A Bot's memory holds at most 4 KiB of `rule` text and 16 KiB of `note` text, counted in UTF-8 bytes.

A write that passes a limit changes nothing, and the Bot says that its memory is full. A write that makes room clears or changes entries in the same request, and the request changes everything or nothing.

A memory write needs no approval, and no Ask or Allow rule covers it.

A template copy carries the Bot's `owner` rules that still apply, including its job, and no `note`, `user`, or `group` entry.

Memory has no on or off switch.

As a conversation grows past what the Model can hold, the Bot summarizes its older parts. Lasting memory rules live in memory and stay.

## Skills

A skill is a reusable set of instructions for how to do a task. A Bot uses the skills its owner enables for it, and a User can name one in a Chat to use it for that request. [Plugin](../plugin/product.md) covers installing skills.

A skill's entry records the Bot or Space it belongs to and where it came from.

A User teaches a Bot a task by showing it once: the User takes control of the Bot's Computer and does the task while the Bot watches and does not act. The record keeps what the User did and what was on the screen, and never keeps what the User typed as a password.

The Bot then writes a draft skill: when to use it, what it needs, its steps, how to check the result, when to ask first, and what to do on failure. The owner reviews, changes, and approves it before the Bot uses it.

After a task with several steps, a User can ask the Bot to save the process as a skill; it becomes a draft the same way. On a Bot shared with other Users, a draft caused by one of them waits for the owner.

## Access

The owner sets who else can use the Bot. The Space lists the Bots a User owns and the Bots they are allowed to use.

A User who is allowed can open a Chat with it and add it to a Group.

A User who is not allowed sees the Bot's name and picture in shared Groups and nothing else.

## Working

A Bot's work is in progress, waiting, or done. While it is in progress, a User redirects it by sending a Message, and stops it by saying so.

A Bot works on one request at a time. A later request waits in a queue, and the Chat shows how many requests are ahead of it.

When work fails, the Bot says so as a Message, and the work is done.

A Bot asks for information, a decision, or an approval in the Chat or Group where the request was made.

## Asking before an action

A Bot does what it was asked without asking. It asks before an action that goes beyond the request, an action the User said to ask about, an action the Computer's owner requires asking for, and an action that its tool asks about by default.

It also asks before the first action it runs on a Local Computer, once for each Bot, acting User, and Local Computer, and before an action a lasting Ask rule names.

Only the User the Bot acts for answers the approval. An Allow runs the action once, and an Always allow also covers later actions of that kind.

An Always allow covers the same tool and either a shell command that starts with the approved command's first word or a file path from the approved action's directory, including files below it. For a tool with no command or path, it covers every action of that tool.

A file at the Workspace root makes the covered directory the Workspace root, so relative paths in the Workspace are covered and absolute ones are not. The first use of a Local Computer offers no Always allow, and no lasting rule covers it.

An Always allow that would grant a folder is refused when the Bot's Computer serves no grants, and the approval still waits for an answer.

An Ask rule makes the Bot ask before a kind of action, and an Allow rule lets it proceed without asking. Which rule wins, which forms a rule cannot judge, and what an Always allow writes follow [design](design.md#approvals).

An Ask or Allow rule belongs to a Bot on a Local Computer and covers only a Computer action, never a memory write. A Bot on a Hosted Computer takes no rule: a Hosted Computer does not ask before an action yet, so the Space refuses one rather than record it.

While a Bot waits for an answer, its other work goes on.

A Deny ends the run, and the Bot says that it stopped.

A request from a run the User started has no time limit. By default, a request from a run no User is with expires after 30 minutes; the Space can change that period.

The Bot does not do an expired action and says so.

A request also ends when the User leaves the conversation, the Computer is revoked, or the run is cancelled.

## Actions that always ask

These actions always need the acting User's Allow, and a lasting rule never covers them:

- Changing the Bot's Ask or Allow rules, its access, its Model, or its Computer.
- Accepting a Bot connection from another owner.
- The first use of a Local Computer.
- The first use of an app on a Computer.
- Paying or spending.
- Filling a saved credential or card.

## Deleting a Bot

The owner deletes a Bot. Its Chats, its memory, and its page go with it.

Its Messages in Groups stay, showing the Bot's name as deleted.

## Schedules and Watchers

A User sets a Schedule or a Watcher by telling the Bot in a Chat or a Group. The Bot creates it.

The Bot's page lists its Schedules and Watchers. The owner edits, pauses, or deletes each one there; telling the Bot in the Chat does the same.

A Schedule or a Watcher starts the Bot's work. The Bot posts the result as a Message in the Chat or Group where the instruction was given, as it does for any request.

A Schedule or a Watcher uses the Bot's Model. A Watcher's check is not shown as work, and the Bot posts only when the condition is met.

A Schedule keeps the time zone of the User who set it.

When a Space was off at the time a Schedule or a Watcher was due, it runs once when the Space is back.

## Templates

The owner shares a Bot as a template: a link that admits any User of the Space to add a copy of the Bot.

| A copy carries | A copy leaves behind |
| --- | --- |
| Name and picture | Its `note`, `user`, and `group` entries |
| The Bot's job and its `owner` rules that still apply | Its Chats |
| The skills the Bot itself wrote | Its Computer and credentials |
| | Its Schedules and Watchers |

## Gaps

- A Question, an unattended approval's expiry, a Group approval, and cancelling an approval are not yet available.
- The built-in approval triggers other than the first use of a Local Computer are not yet available.
- A Hosted Computer does not ask before an action yet.
- The Bot's page does not show approvals yet.
- An independent reviewer of approvals is not yet specified.
- Group memory, its suggestions, and their approval are not yet available.
- A template shared across Spaces is not yet specified.
- Changing a Bot's picture is not yet available; a Bot's picture is the initials of its name, and a User cannot upload or replace one.
- Changing who else can use a Bot is not yet available; it arrives when a Bot is shared among several Users.
