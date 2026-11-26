# Computer

## Authority

Owned by computer. A Computer depends on [Space](../space/product.md) to be reached and on [Bot](../bot/product.md) for who lives on it.

## Laws

1. A new Bot starts on a real Computer of its owner, and otherwise on a managed one.
2. A real Computer runs Neha and connects out to the Space.
3. A Bot does what its Computer can do.
4. Where Neha cannot set up an isolated environment, a Space has no managed Computer.

## Kinds

| Kind | Who provides it | What it has |
| --- | --- | --- |
| Managed | Neha, on the Space's own machine | An isolated environment Neha sets up, with files of its own, a terminal, and a browser |
| Real | The user, by installing Neha on a machine | The machine's own files, logins, and software |

## Keeping files

A managed Computer keeps its files when it stops and after a recovery.

Recovering or resetting a managed Computer builds a new machine from the last saved files; work since that save is lost.

A real Computer keeps its files on the machine, and the Space keeps no copy.

## Assigning a Computer

A Bot's page shows its Computer. The owner picks a managed one or one of the real Computers the owner owns.

Only a Bot's owner can move it from one of their real Computers to another real Computer they own. [Bot](../bot/product.md#moving-a-bot) covers what the Bot takes with it and what stays on the old Computer.

Several Bots can be assigned the same Computer, and then share its files and logins. The Bots on one Computer are not isolated from each other.

## Adding a real Computer

A User adds a real Computer in one of two ways.

They install Neha on the machine and start its pairing there, and the machine shows a code. A User signed in to the Space approves the pairing with that code. [Identity](../identity/product.md) covers the pairing, and [CLI](../cli/product.md) covers the commands.

The machine serves the Space while it runs; after a stop, the User starts it again.

In the desktop app, the User turns on the machine the app runs on. No code is needed, because the User is already signed in. The app asks when the first Bot is created.

The User who added the machine owns that Computer. They see it among their real Computers and can assign their Bots to it; other Users do not see it.

A paired Computer connects out to the Space and shows as online or offline.

A Computer that is offline keeps its Bots and their memory, and its Bots work again when it comes back.

A Message sent to a Bot waits while the Computer assigned to it is offline. Waiting work starts when that Computer is online, including after a move to another online Computer. A run that already started keeps the Computer it started on; [design](design.md#moving-a-bot) covers the move, and the Chat shows the offline Computer.

## The panel

A Chat with a Bot shows its Computer's screen as a panel. The panel shows the screen live, what the Bot is doing, and whether the user has control. On a managed Computer, the screen is the browser's.

| Action | What happens |
| --- | --- |
| Watch | The user sees the screen; the Bots on it keep working |
| Take control | The user gets the keyboard and mouse; every Bot on that screen pauses, and their other work continues |
| I'm done | The Bots on that screen resume |

On a real Computer, the User who touches its own keyboard or mouse takes control as well. The panel shows it, and I'm done gives the screen back.

A Bot can ask the User to take control, for example to sign in.

## What a Bot can do

A Bot runs commands, reads and writes files, and operates the screen of its Computer. On a managed Computer, a Bot operates the browser. Programs with windows of their own run only on a real Computer.

On a real Computer, a Bot starts in its own Workspace on that machine, and its default write range is that Workspace. It reads what the machine's user can read, except the Computer's own configuration directory, which [design](design.md#isolation) hides from it.

On a Computer that several Bots share, one Bot at a time controls the keyboard and mouse.

On a real Computer, the owner sets its access: granted folders, or full access. With granted folders, a Bot writes and deletes files only in those folders, and its read range is unchanged. With full access, it writes and deletes what the machine's user can.

A new real Computer starts with granted folders.

A Bot delivers a file of its Workspace as a File on a Message it sends, and the File lives in the Space from then on. A Bot also takes a File of its conversation into its Workspace. [Message](../messaging/message/product.md#files) covers Files.

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
- Moving a Bot from a managed Computer to a real one is not yet specified, including how its Workspace is carried.
