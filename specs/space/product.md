# Space

## Authority

Owned by space. Every other topic depends on a Space to hold its record.

## Laws

1. Every User belongs to a Space.
2. The record lives in the Space and nowhere else.
3. The Admin configures the model providers a Space offers.

## Deliverables

| Deliverable | Platforms | Role |
| --- | --- | --- |
| Desktop app | Windows first | Client; runs a built-in Space or connects to a remote one |
| Android app | Android first, iOS later | A full client; connects to a remote Space |
| Web | Any browser | A full client, with everything the desktop app does |
| Server | Linux, Windows | A Space the user deploys, with no interface of its own |
| Hosted Space | Run by Neha | The same Space, run for a user who does not want to run a server; the contract is identical |
| CLI | Linux headless and every other platform | Everything the GUI does, for a user or an agent; see [cli](../cli/product.md) |

A client connects to one Space at a time.

## Starting

The desktop app opens with a choice: use the Space built into this computer, or connect to a remote one.

The built-in Space is created on first use, and its first user is its Admin.

A new Space has no Provider. Connecting one is the first thing its Admin does, before any Bot can work; see [Model](../model/product.md).

The phone app connects to a remote Space.

## Closing the desktop app

Closing the desktop app's window keeps its built-in Space running in the background. A User can set the app to exit when its window closes.

## Connecting

A User connects to a remote Space with an invitation link: the first from the Space, the rest from its Admin. [Identity](../identity/product.md) covers invitations, accounts, and sign-in.

A connected client shows the User's Bots, Chats, and Groups, the same on every client.

## Who can read the record

Whoever runs a Space can read its record and what passes through it from a Computer. A built-in Space is run by its first User, and a hosted Space is run by Neha.

## Users

Users join and leave a Space as [Identity](../identity/product.md) describes.

A User who is removed loses access to the Space; their Bots stay until the Admin deletes them.

## Model providers

The Admin connects the Space's model providers. [Model](../model/product.md) covers connecting one, what a Model can do, and how a Bot chooses.

## Storage

The Admin decides where the Space keeps the bytes of its Files.

## Usage budget

The Admin sets a usage budget for the Space or for one Bot, over a day, a month, or no period.

A budget counts normalized tokens: uncached input, cache reads, cache writes, and output. [Model](../model/design.md) covers the one normalized form of usage.

A Space budget and a Bot budget both apply, and the stricter one stops work.

When the budget is reached, no new run starts, and a run in progress stops between two actions. The Bot says so as a Message.

A Space with no budget set places no limit.

## Notifications

A Space delivers Notifications to a User's clients. [Notification](../messaging/notification/product.md) covers what starts one and how a User controls it.

## Deleting a Space

The Admin can delete the Space, which deletes its record, Bots, and Users.

## Gaps

- Exporting the Space's record, for backup, is not yet specified.
- Reaching Neha from outside its apps, by sharing into it from any app or calling it with a global shortcut, is not yet specified and comes after the CLI.
