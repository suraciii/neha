# Computer

## Authority

Owned by computer. A Computer depends on [Space](../space/product.md) to be reached and on [Bot](../bot/product.md) for who lives on it.

## Laws

1. A new Bot's Computer is a managed one.
2. A real Computer runs Neha and connects out to the Space.
3. A Bot does what its Computer can do.

## Kinds

| Kind | Who provides it | What it has |
| --- | --- | --- |
| Managed | Neha, on the Space's own machine | An isolated environment Neha sets up |
| Real | The user, by installing Neha on a machine | The machine's own files, logins, and software |

## Assigning a Computer

A Bot's page shows its Computer. The owner picks a managed one or one of the real Computers the owner owns.

Several Bots can be assigned the same real Computer, and then share its files and logins.

## Adding a real Computer

A User installs Neha on a machine and pairs it with the Space: the machine shows a code, and the User enters it in an app. [Identity](../identity/product.md) covers the pairing.

The User who paired the machine owns that Computer. They see it among their real Computers and can assign their Bots to it; other Users do not see it.

A paired Computer connects out to the Space and shows as online or offline.

A Computer that is offline keeps its Bots and their memory, and its Bots work again when it comes back.

A Message sent to a Bot whose Computer is offline waits until the Computer is back. The Chat shows the Computer as offline.

## The panel

A Chat with a Bot shows its Computer's screen as a panel. The panel shows the screen live, what the Bot is doing, and whether the user has control.

| Action | What happens |
| --- | --- |
| Watch | The user sees the screen; the Bot keeps working |
| Take control | The user gets the keyboard and mouse; the Bot pauses on that screen, and its other work continues |
| I'm done | The Bot resumes on that screen |

## What a Bot can do

A Bot runs commands, reads and writes files, and operates the screen of its Computer.

A File a Bot produces is attached to a Message and lives in the Space from then on.

## Credentials

On a real Computer, a Bot uses the machine's own logins: its browser sessions, its keys, and its configuration, as the user set them up.

On a managed Computer, a Bot gets credentials in one of two ways.

| Way | What happens |
| --- | --- |
| Vault | The User stores a password or key in the Space's Vault, and the Bot uses it without seeing it |
| Take control | The User signs in by taking control of the Computer, and the session stays on that Computer |

A credential never appears in a Chat or a Group.

## Gaps

- Waking a sleeping Computer is not yet specified and is low in priority.
