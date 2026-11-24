# Group

## Authority

Owned by messaging. A Group depends on [Message](../message/product.md) for its stream, on [Chat](../chat/product.md) for the state it shares with a Chat, and on [Bot](../../bot/product.md) for who can add a Bot.

## Laws

1. Whatever a member says in a Group is in the Group's record.
2. A Bot in a Group acts for the User whose request it is working on.
3. A Group's Bots are Bots its members are allowed to use.

## Creating a Group

A User creates a Group with a name and any number of members: Users of the Space, and Bots the creator is allowed to use.

The Group's owner adds and removes members at any time. A User adds only Bots they are allowed to use.

## The stream

A Group shows its Messages oldest first, and every member reads the same record.

A User addresses one Bot with `@` and its name, every Bot with `@all`, and nobody by writing without an address.

## Who answers

An addressed Bot answers. Every Bot answers an `@all`.

A Bot that is not addressed answers when it judges the Message is its own to answer. Several Bots can answer one Message, and their answers land in the order they arrive.

A Bot's handoff to another Bot is a Message in the Group that addresses that Bot, and the addressed Bot acts on it.

## State and approvals

A Group's row in the sidebar shows the Group's name, its members, the latest Message, and the same states a Chat shows.

An Approval or a Question appears in the Group where the request was made, and the User the Bot acts for answers it.

## Leaving and end

A member leaves a Group at any time. The owner does not leave; the owner deletes the Group.

A Group ends when its owner deletes it, and its Messages go with it.
