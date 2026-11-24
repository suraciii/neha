# Bot

## Authority

Owned by bot. A Bot depends on [Computer](../computer/product.md) for where it lives, on [Chat](../messaging/chat/product.md) for its conversation with its owner, and on [Space](../space/product.md) for the Users it can be shared with.

## Laws

1. A Bot has exactly one owner.
2. A Bot's memory is its own and goes where the Bot goes.
3. A new Bot starts on a managed Computer.

## Creating a Bot

A new Space shows an empty sidebar with one action: create a Bot.

A User creates a Bot with a name and a picture. The Bot starts on a managed Computer, and its Chat with the owner opens.

The owner gives the Bot its job and its rules in that Chat, and asks it to remember them.

A Bot works once the Space has a Provider.

## The Bot's page

A Bot has a page with its name, picture, Computer, access, and Model. The owner edits all of them; other Users read them.

| Field | What it holds | Who changes it |
| --- | --- | --- |
| Name | The Bot's name | The owner |
| Picture | The Bot's avatar | The owner |
| Computer | The managed or real Computer it lives on | The owner |
| Access | Who else can use it: only the owner, everyone, or named Users | The owner |
| Model | The Model it thinks with and its thinking level; the Space's default unless the owner chooses | The owner |

[Model](../model/product.md) covers connecting Providers and choosing a Model.

## Memory

A Bot keeps what it learns in its Chats and Groups, and carries it into every conversation.

When a User asks a Bot to remember something, the Bot keeps it as a lasting rule.

A User sees what a Bot remembers by asking it in the Chat.

As a conversation grows past what the Model can hold, the Bot summarizes its older parts. Lasting rules live in memory and stay.

## Access

The owner sets who else can use the Bot. The Space lists the Bots a User owns and the Bots they are allowed to use.

A User who is allowed can open a Chat with it and add it to a Group.

A User who is not allowed sees the Bot's name and picture in shared Groups and nothing else.

## Working

A Bot's work is in progress, waiting, or done. While it is in progress, a User redirects it by sending a Message, and stops it by saying so.

When work fails, the Bot says so as a Message, and the work is done.

A Bot asks for information, a decision, or an approval in the Chat or Group where the request was made.

## Deleting a Bot

The owner deletes a Bot. Its Chats, its memory, and its page go with it.

Its Messages in Groups stay, showing the Bot's name as deleted.

## Schedules and Watchers

A User sets a Schedule or a Watcher by telling the Bot in a Chat or a Group. The Bot creates it.

The Bot's page lists its Schedules and Watchers. The owner edits, pauses, or deletes each one there; telling the Bot in the Chat does the same.

A Schedule or a Watcher starts the Bot's work. The Bot posts the result as a Message in the Chat or Group where the instruction was given, as it does for any request.

## Templates

The owner shares a Bot as a template: a link that admits any User of the Space to add a copy of the Bot.

| A copy carries | A copy leaves behind |
| --- | --- |
| Name and picture | Memory |
| The Bot's job and rules | Its Chats |
| | Its Computer and credentials |
| | Its Schedules and Watchers |

## Gaps

- A taught procedure, shown once and repeated, is parked until skills are designed.
- A template shared across Spaces is not yet specified.
