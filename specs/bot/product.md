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

A Bot has a page with its name, picture, Computer, access, and Model. The owner edits all of them; other Users read them.

| Field | What it holds | Who changes it |
| --- | --- | --- |
| Name | The Bot's name | The owner |
| Picture | The Bot's avatar | The owner |
| Computer | The Hosted or Local Computer it lives on | The owner |
| Access | Who else can use it: only the owner, everyone, or named Users | The owner |
| Model | The Model it thinks with and its thinking level; the Space's default unless the owner chooses | The owner |

[Model](../model/product.md) covers connecting Providers and choosing a Model.

## Moving a Bot

[Computer](../computer/product.md#assigning-a-computer) covers who can move a Bot and where. Its memory, its Chats, and the Files it delivered stay with it, and it starts in its own Workspace on the new Computer. Work it did not deliver stays on the old Computer.

## Memory

A Bot keeps what it learns as memory entries. The default scope is the owner's Chats; a Chat entry belongs to that Chat's User, and a Group entry belongs to the Group.

| Scope | Applies in | Who reads it | Who clears it |
| --- | --- | --- | --- |
| Owner | The owner's Chats | The owner | The owner |
| User | One other User's Chat | That User | That User, and the owner |
| Group | The Group where it was learned | Every member of the Group | Its author, and the owner |

When a User asks a Bot to remember something, it keeps the entry at the scope of that conversation. In a Group, an entry from a non-owner waits as a suggestion.

A suggestion applies to nothing until the owner approves it. A rule the owner gives in a Group applies to the Group.

The owner can audit every entry's scope, author, and source without reading a User entry's content, and can clear any entry. A User sees and clears the entries that are theirs by asking the Bot in the Chat.

As a conversation grows past what the Model can hold, the Bot summarizes its older parts. Lasting rules live in memory and stay.

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

The User it acts for answers with Allow or Deny. In a Chat, the User can make an Allow last for that kind of action by saying so, and the Bot keeps it as a lasting rule.

While a Bot waits for an answer, its other work goes on.

A request from a run the User started has no time limit. By default, a request from a run no User is with expires after 30 minutes; the Space can change that period.

The Bot does not do an expired action and says so.

A request also ends when the User leaves the conversation, the Computer is revoked, or the run is cancelled.

## Actions that always ask

These actions always need the acting User's Allow, and a lasting rule never covers them:

- Changing the Bot's rules or instructions.
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
| Name and picture | Memory |
| The Bot's job and rules | Its Chats |
| The skills the Bot itself wrote | Its Computer and credentials |
| | Its Schedules and Watchers |

## Gaps

- A template shared across Spaces is not yet specified.
