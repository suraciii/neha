# Computer

## Authority

Owned by computer. A Computer depends on [Space](../space/product.md) to be reached and on [Bot](../bot/product.md) for who lives on it.

## Laws

1. A new Bot starts on a Hosted Computer, unless its owner creates it on a Local Computer.
2. A Local Computer runs Neha and connects out to the Space.
3. A Bot does what its Computer can do.
4. Where Neha cannot set up an isolated environment, a Space has no Hosted Computer.

## Kinds

| Kind | Who provides it | What it has |
| --- | --- | --- |
| Local | The User, on the machine where Neha runs | The machine's own files, logins, and software |
| Hosted | Neha, on the Space's own machine | An isolated environment Neha sets up, with files of its own, a terminal, and a browser |

## Keeping files

A Hosted Computer keeps its Bot's Workspace in the Space, and a restart of the Space keeps it.

Resetting a Hosted Computer clears its Bot's Workspace: every file and folder inside it is deleted, and the Workspace folder stays.

Only the Computer's owner resets it.

A Local Computer keeps each Bot's Workspace on the machine, and the Space keeps no copy.

## Assigning a Computer

A Bot's page shows its Computer. The owner picks a Hosted Computer, or creates the Bot on the Local Computer where Neha runs.

Only a Bot's owner can move it from one of their Local Computers to another Local Computer they own. [Bot](../bot/product.md#moving-a-bot) covers what the Bot takes with it and what stays on the old Computer.

Several Bots can be assigned the same Computer, and then share its files and logins. The Bots on one Computer are not isolated from each other.

## Adding a Local Computer

A User adds the Local Computer of the machine where Neha runs. Joining needs no code: the Space issues a local ticket, and the User starts the pairing there and confirms it.

A Local Computer on the machine where Neha runs is the only Local Computer this contract covers. [Identity](../identity/design.md#codes) covers the ticket, and [CLI](../cli/product.md) covers the commands.

The Computer belongs to the User the ticket names; that owner sees it among their Local Computers and can assign their Bots to it, and other Users do not see it.

The machine serves the Space while it runs; after a stop, the User starts it again.

A paired Computer connects out to the Space and shows as online or offline.

A Computer that is offline keeps its Bots and their memory, and its Bots work again when it comes back.

A Message sent to a Bot waits while the Computer assigned to it is offline. Waiting work starts when that Computer is online, including after a move to another online Computer. A run that already started keeps the Computer it started on; [design](design.md#moving-a-bot) covers the move, and the Chat shows the offline Computer.

## The panel

A Chat with a Bot shows its Computer's screen as a panel. The panel shows the screen live, what the Bot is doing, and whether the user has control. On a Hosted Computer, the screen is the browser's.

| Action | What happens |
| --- | --- |
| Watch | The user sees the screen; the Bots on it keep working |
| Take control | The user gets the keyboard and mouse; every Bot on that screen pauses, and their other work continues |
| I'm done | The Bots on that screen resume |

On a Local Computer, the User who touches its own keyboard or mouse takes control as well. The panel shows it, and I'm done gives the screen back.

A Bot can ask the User to take control, for example to sign in.

## What a Bot can do

A Bot runs commands, reads and writes files, and operates the screen of its Computer. On a Hosted Computer, a Bot operates the browser. Programs with windows of their own run only on a Local Computer.

A Bot on a Hosted Computer works only in its Workspace.

On a Local Computer, a Bot starts in its own Workspace on that machine, and its default write range is that Workspace. It reads what the machine's user can read, except the Computer's own configuration directory, which [design](design.md#isolation) hides from it.

On a Computer that several Bots share, one Bot at a time controls the keyboard and mouse.

On a Local Computer, the owner sets its access: granted folders, or full access. With granted folders, a Bot writes and deletes files only in those folders, and its read range is unchanged. With full access, it writes and deletes what the machine's user can.

A new Local Computer starts with granted folders.

A Bot delivers a file of its Workspace as a File on a Message it sends, and the File lives in the Space from then on. A Bot also takes a File of its conversation into its Workspace. [Message](../messaging/message/product.md#files) covers Files.

## Credentials

On a Local Computer, a Bot uses the machine's own logins: its browser sessions, its keys, and its configuration, as the user set them up.

On a Hosted Computer, a Bot gets credentials in one of two ways.

| Way | What happens |
| --- | --- |
| Vault | The User stores a password or key in the Space's Vault, and the Bot uses it without seeing it |
| Take control | The User signs in by taking control of the Computer, and the session stays on that Computer |

A credential never appears in a Chat or a Group.

## Gaps

- A provider-backed Hosted Computer is not yet specified, including its start, stop, recovery, and destruction.
- Saving a Workspace and restoring one are not yet specified.
- A Hosted Computer has no screen, browser, Vault credential, or Take control yet.
- Waking a sleeping Computer is not yet specified and is low in priority.
- Revoking a Computer's credential is not yet specified.
- Adding a Computer on another machine is not yet specified.
- Moving a Bot to or from a Hosted Computer is not yet specified, including how its Workspace is carried.
