# Memory

A Bot keeps what it learns as memory. This guide shows the two kinds of memory entry, the scope each one applies in, how to read and clear them, and how memory differs from approval rules. It also shows what happens when a Chat grows long. It follows [Quick start](quickstart.md), so the Space, the Admin, the Provider, the Bot, and the Chat already exist.

## What memory is

A Bot keeps what it learns as memory entries. An entry has one of two kinds:

| Kind | What it is | What writes it |
| --- | --- | --- |
| `rule` | A lasting instruction | A User's words in a Chat |
| `note` | What the Bot noticed, kept as data | The Bot |

Only a User's words start a `rule`: the Bot never starts one on its own. The Bot writes a `note` when it decides to keep something, and it can merge or remove its own notes. When you ask the Bot to remember something in a Chat, the entry it keeps is a `rule`.

A `rule`'s author is the User whose words gave it, and a `note`'s author is the Bot. An entry's source names the Message, run, or command that wrote it.

The Bot's job is its earliest `rule` in `owner` scope that still applies. Tell it its job before its other rules: a rule kept earlier would become the job instead.

The Bot reads a `rule` as an instruction and a `note` as data. Ask it to remember only what you want it to follow.

Neha does no background pass over a Chat to extract memory. The Bot writes an entry during a run, and it reads the entries that apply each time it works, so a new entry takes effect on the next request.

Memory belongs to the Bot, not to a Chat. It stays with the Bot when the owner [moves it](computer.md#moving-a-bot) to another Local Computer.

## Where an entry applies

The default scope is the owner's Chats. An entry applies only in its scope.

| Scope | Applies in | Who reads it | Who clears it |
| --- | --- | --- | --- |
| `owner` | The owner's Chats | The owner | The owner |
| `user` | The Chat between the Bot and that User | That User | That User, and the owner |

A Group entry belongs to the Group where it was learned. Group memory is not available yet.

When you own the Bot and ask it to remember something in your Chat, the entry is in `owner` scope and applies in the owner's Chats. A `user` entry belongs to the Chat between the Bot and one other User, and applies only in that Chat. Sharing a Bot with other Users is not available yet.

The owner sees every entry's scope, author, and source without reading another User's content. A User reads and clears the entries that are theirs, including the `note` entries the Bot wrote while acting for that User.

## How much a Bot can keep

A Bot's memory holds at most 4 KiB of `rule` text and 16 KiB of `note` text, counted in UTF-8 bytes. Each kind has its own limit.

A write that passes a limit changes nothing, and the Bot says that its memory is full. To keep something when a kind is full, the Bot can remove or replace entries of that kind in the same request. A request that still passes the limit changes nothing. Clear an entry on the Bot's page or from the CLI to make room.

The same text written again for the same Bot, kind, scope, and acting User changes nothing; a retry adds no second entry. Two Users who write the same text keep one entry each.

## Long conversations

As a Chat grows long, the Bot summarizes its older Messages, so it can keep working on the same conversation. The summary is not a Message in the Chat.

The Chat keeps every Message, including the ones the summary covers. Where a compacted part of the Chat ends, it shows the marker **Earlier Messages were compacted**. The marker has no action and the summary is not shown. The marker comes from the record, so a reload shows it in the same place. If the Bot summarizes more Messages later, the mark moves to the new boundary.

`neha chat read <chat>` prints the Chat's Messages oldest first. Where the compacted part ends, it prints a marker that says earlier Messages were compacted, and it prints no summary.

Compaction does not remove memory. The Bot's rules and notes stay, and a lasting rule still applies after its Chat is compacted. Before it compacts, the Bot can write notes for what it has not written down yet.

The Bot compacts when its conversation needs it, and you take no step for it.

## Read and clear memory

### In the browser

Open Bots and select the Bot. [Manage a Bot in the browser](quickstart.md#manage-a-bot-in-the-browser) covers the page.

The Memory section lists the entries you read, grouped by kind and scope. Each entry names its author, its source, and its size, and shows its content.

The owner sees every entry. An entry the owner does not read shows only its scope, author, source, and size.

Each row offers removal to the User who can clear that entry, so the owner can remove any entry. The section shows each kind's used size and its limit. With no entry, it says to ask the Bot in a Chat to remember something.

The page adds and changes no entry. You add and change memory in a Chat.

### From the CLI

The owner reads a Bot's memory from the CLI. A User allowed to use a shared Bot will read it there once sharing is available. Copy the Bot's id from `neha bot list` into `BOT_ID`, then list its memory:

```bash
target/debug/neha bot list
BOT_ID="<the Bot's id>"
target/debug/neha bot memory "$BOT_ID"
```

Each entry prints its id, kind, scope, author, source, size, and content. The owner also sees every entry the owner does not read, with metadata only. The command prints each kind's total used size and its limit. With no entry, it says to ask the Bot in a Chat to remember something.

Remove an entry by the id the listing prints:

```bash
ENTRY_ID="<the entry's id>"
target/debug/neha bot memory remove "$BOT_ID" "$ENTRY_ID"
```

The owner can remove any entry. The command refuses an entry the User running it cannot clear.

### In a Chat

The Bot adds and changes memory in a Chat. Give it its job first, then the other rules it should follow:

| Say in the Chat | What it does |
| --- | --- |
| "Remember that your job is to keep my release notes up to date." | Gives the Bot its job: its earliest `rule` in `owner` scope |
| "Remember that I prefer short answers." | Keeps another `rule`; the job stays the earliest one |
| "What do you remember?" | Answers from the entries that apply |
| "Forget that I prefer short answers." | Removes an entry the User can clear |

The Bot answers from the entries that apply to that request, and it removes an entry when the User asking can clear it.

## Memory rules and approval rules

Both use the word rule, and they do different work. A memory `rule` is an instruction: it tells the Bot how to behave. An Ask or Allow rule is a permission: it decides whether the Bot asks before a Computer action.

For a Local Bot, approval rules are on the Bot's page under Access, and `neha rule` manages them. [Approvals](approvals.md) covers them. Memory entries are in the Memory section, and `neha bot memory` reads them.

Always allow writes no memory. See [Approvals](approvals.md) for its lasting effect.

A memory write needs no approval. No Ask or Allow rule covers it, and it is not a Computer action.

To teach the Bot a lasting preference, ask it in a Chat to remember the preference. To let an action proceed without asking, add an Allow rule.

## Limits

- Memory has no on or off switch.
- Sharing a Bot as a template is not available. A template copy carries the Bot's `owner` rules that still apply, including its job, and no `note`, `user`, or `group` entry.
- Deleting a Bot is not available. It takes its Chats, its memory, and its page with it.
