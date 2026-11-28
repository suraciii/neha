# Computer

## Authority

Owned by computer. A Computer depends on [Space](../space/product.md) to be reached and on [Bot](../bot/product.md) for who lives on it.

## Laws

1. A new Bot starts on its owner's Space Computer, unless its owner creates it on another Computer.
2. A Local Computer runs Neha and connects out to the Space.
3. A Bot does what its Computer can do.
4. Where Neha cannot set up an isolated environment, a Space has no Hosted Computer.
5. A Hosted Computer MAY be asleep, and work wakes it.

## Kinds

| Kind | Who provides it | What it has |
| --- | --- | --- |
| Local | The User, on a machine they control | The machine's own files, logins, and software |
| Hosted | Neha, on the Space's own machine | An isolated environment Neha sets up, with files of its own, a terminal, and a browser |

A User owns each Hosted Computer they create or receive. The Space Computer is the Hosted Computer every User receives by default.

A Bot's loop and record live in its Space; its actions run on its Computer.

## Keeping files

A Computer keeps one Workspace. A Hosted Computer keeps its Workspace in the Space, and a restart of the Space keeps it.

Each Bot works in its own directory inside its Computer's Workspace. A Bot on a Hosted Computer starts in that directory; the sandbox mounts the whole Computer Workspace, so Bots on the same Hosted Computer can read and write each other's files.

Resetting a Hosted Computer clears every Bot's directory in its Workspace: every file and folder inside it is deleted, and the Workspace root stays.

Only the Computer's owner resets it.

A Local Computer keeps its Workspace on the machine, and the Space keeps no copy. Each Bot works in its own directory there, as it does on a Hosted Computer.

## Assigning a Computer

A Bot's page shows its Computer. A new Bot starts on its owner's Space Computer unless its owner names another Computer they own.

A User owns the Hosted Computers they create or receive. They can create a Hosted Computer that is shared or exclusive:

| Sharing | Who MAY use it |
| --- | --- |
| Shared | Any of the owner's Bots |
| Exclusive | One Bot; another Bot MAY use it only after the owner makes it shared |

The owner creates a Bot on a Computer they own, and can move a Bot between their Computers. Moving works between Hosted and Local Computers in either direction. [Bot](../bot/product.md#moving-a-bot) covers what the Bot takes with it and what stays on the old Computer.

Several Bots can be assigned the same shared Computer, and then share its files and logins. On a Local Computer they also share the machine's processes; on a Hosted Computer each run's shell has its own process namespace, while the Computer Workspace is shared. The Bots on one Computer are not isolated from each other.

## Adding a Local Computer

A User adds a Local Computer they control. A signed-in client makes that machine available, and the Space pairs it with that User; [Identity](../identity/product.md#devices) covers the code.

A Local Computer MAY run on the machine that runs the Space or on another machine that can reach the Space. It connects out to the Space and listens on no network port.

The Computer belongs to the User the pairing names; that owner sees it among their Local Computers and can assign their Bots to it, and other Users do not see it.

The machine serves the Space while it runs; after a stop, the User starts it again.

A paired Computer connects out to the Space and shows as online or offline. A worker-backed Hosted Computer also shows as online or offline; it is offline while its worker is asleep.

A Computer that has not greeted yet names no access and no fence: the pairing answer and every later listing agree on that.

A Computer that is offline keeps its Bots and their memory, and its Bots work again when it comes back.

A Message sent to a Bot waits while the Computer assigned to it is offline. Waiting work starts when that Computer is online, including after a move to another online Computer, and a Message wakes a worker-backed Hosted Computer.

A run that already started keeps the Computer it started on; [design](design.md#moving-a-bot) covers the move, and the Chat shows the offline Computer.

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

A Bot on a Hosted Computer starts in its own directory inside its Computer's Workspace, and its shell can reach the rest of that Workspace.

On a Local Computer, a Bot starts in its own directory inside its Computer's Workspace, and its default write range is that directory. It reads what the machine's user can read. [Design](design.md#isolation) hides the Computer's own configuration directory from a Bot with granted folders.

A Windows Local Computer is an explicitly unsandboxed mode. Its commands run with the machine user's rights, not only in the Bot's folder; the owner accepts a warning before Neha starts it, and the Space operator opts in on the wire. Grants, full access, and a run directory are unavailable in this mode.

On a Computer that several Bots share, one Bot at a time controls the keyboard and mouse.

On a Linux Local Computer, the owner sets its access: granted folders, or full access. A Windows Local Computer has no such access model.

A Local Computer states which access it supports, and a client offers only what the Computer states. The browser disables the controls a Computer does not support, and the CLI refuses a command that asks for them and names what the Computer does not support. An owner always revokes a grant and turns full access off.

A Local Computer also states whether it fences a Bot's run. A Computer that serves no fence runs a Bot's shell with the machine user's own rights, and one that has not answered yet is shown as waiting rather than named either way.

With granted folders, a Bot writes and deletes files only in its Workspace, the granted folders, and the run's own working directory. With full access, Neha starts no sandbox, and the Bot writes, deletes, and reads with the machine user's own file rights, including the Computer's configuration directory.

The approval gate applies to full access as it does to granted folders. [Bot](../bot/product.md#asking-before-an-action) covers when a Bot asks.

Full access lets a Bot read the saved pairing, the machine's token, and the CLI's session credential in that directory. The owner accepts that risk when they turn full access on.

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

- There is no User control to start, stop, or wake a Hosted Computer. A containerized deployment wakes a worker from work and stops it when idle; a process-local Hosted Computer is always awake.
- Deleting a Hosted Computer is not yet specified.
- Removing the last Bot from a Hosted Computer does not remove its worker or Workspace root.
- Saving a Workspace and restoring one are not yet specified.
- A Hosted Computer has no screen, browser, Vault credential, or Take control yet.
- Revoking a Computer's credential is not yet specified.
