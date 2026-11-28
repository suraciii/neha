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
| Desktop app | Windows first | Client; starts a Local Space or connects to a Space |
| Android app | Android first, iOS later | A full client; connects to a Space |
| Web | Any browser | A full client, with everything the desktop app does |
| Server | Linux, Windows | Runs a Hosted Space; no interface of its own |
| Hosted Space (Neha-run) | Run by Neha | The same Space, operated by Neha for a User who does not want to run a server |
| CLI | Linux headless and every other platform | Everything the GUI does, for a user or an agent; see [cli](../cli/product.md) |

## Deployment modes

A Space runs as a Local Space or a Hosted Space. The contract is the same.

A Local Space runs on the computer where the desktop app starts it. It is created on first use, and its first User creates the Admin account. Another client, including a phone, reaches it only through an origin that computer exposes and that the other client can reach.

A Hosted Space runs on a server. Its operator supplies the server and its origin; a Neha-run Hosted Space is the same contract with Neha as operator. Its Users reach it over the network.

A Hosted Space MAY run in containers. A containerized Hosted Space puts only its TLS proxy on a public port; the Space, its Supervisor, and its Hosted Computer workers run on private Docker networks.

The deployment mode changes where a Hosted Computer runs, not what a User sees: the record, Bots, Chats, Files, and the Computer channel keep the same contract.

A Hosted Computer in a containerized Hosted Space MAY be offline between runs while its worker is asleep; a Message waits for it to wake, as it waits for an offline Local Computer.

A client connects to one Space at a time.

## Starting

The desktop app opens with a choice: start a Local Space on this computer, or connect to a Space.

A Local Space is created on first use; its first User creates the Admin account through the invitation page.

A new Space has no Provider. Connecting one is the first thing its Admin does, before any Bot can work; see [Model](../model/product.md).

The phone app connects to a Space. A Hosted Space is reachable over the network; a Local Space is reachable when the computer that runs it exposes a reachable origin.

## Closing the desktop app

Closing the desktop app's window keeps its Local Space running in the background. A User can set the app to exit when its window closes.

## Connecting

A User connects to a Space with an invitation link: the first from the Space, the rest from its Admin. [Identity](../identity/product.md) covers invitations, accounts, and sign-in.

A connected client shows the User's Bots, Chats, and Groups, the same on every client.

## Who can read the record

Whoever runs a Space can read its record and what passes through it from a Computer. A Local Space is run by its first User; a Hosted Space is run by its operator. A Neha-run Hosted Space is run by Neha.

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
