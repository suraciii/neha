# Identity

## Authority

Owned by identity. [Space](../space/product.md) holds the accounts; [CLI](../cli/product.md) and [Computer](../computer/product.md) join through it.

## Laws

1. Every User of a Space has an account on it.
2. A Local Space's first User creates its account through the invitation page; no client creates or signs in an account silently.
3. A device joins a Space through a code that a signed-in User enters. A Local Computer on the Space's own machine joins through a local ticket instead.

## Accounts

An account has a display name, a picture, and one or more ways to sign in. An email address is optional and never the account's identity.

The first User of a Space becomes its Space Owner. A Local Space creates the first account through the invitation page; a Hosted Space creates it with the invitation link that it prints when it starts.

## Roles

A Space has one Space Owner, any number of Admins, and every other User is a Member.

| Role | What the User MAY do |
| --- | --- |
| Space Owner | Make an Admin, change another User's role, transfer the Space, and do everything an Admin does |
| Admin | Invite Users, connect Providers, install Plugins, change the Space's settings, and revoke invitations |
| Member | Use the Space, read Settings, and own their own Bots, Chats, and Computers |

Every signed-in User MAY read the Space's Users and their roles. Only the Space Owner changes a role, and only the Space Owner transfers the Space. The Space Owner cannot be demoted.

The record holds at most one Space Owner. A transfer makes its receiver the Space Owner and its caller an Admin in one event, so no moment leaves two Owners or none.

## Signing in

| Way | When the Space offers it |
| --- | --- |
| Passkey | The Space has a domain and HTTPS |
| Password | Every Space reached over the network |
| An external provider | An Admin has added one as a Plugin |

A signed-in client stays signed in until the User signs out or removes that client, or the Admin removes the account. This milestone's clients sign out on their own.

A User can see the clients signed in as them and remove any one of them once the device list arrives in a later milestone; this milestone has no device list, so a client leaves only by signing out.

## Inviting

The Space Owner or an Admin makes an invitation link, which admits exactly one new User to the Space as a Member. The link also shows as a QR code, which admits the same one User.

An invitation expires after 7 days, and a used or revoked invitation admits no one. The Space shows the link once, in the answer that makes it, and stores only a hash of its secret; no stored answer, event, or account holds the secret in plain text.

The Space lists the pending invitations with the time each expires, and an Admin revokes one there. Making a new link replaces every earlier link, including one whose join failed.

The invited User opens the link in an app or a browser, sets a display name and a password, and joins as a Member.

Every Space offers invitations. A Local Space's link works only from a device that can reach that machine.

## Devices

The CLI signing in shows a code.

The terminal runs `neha login --device-code`, prints the code, and waits. A signed-in User enters the code in an app or a browser, or with `neha device approve`, and the CLI joins the Space on that User's behalf and acts as the User.

The code admits one terminal token once, expires after fifteen minutes, and never returns the token itself. The Space keeps only the token's hash, and the terminal saves its credential only after the Space accepts it for an authenticated command.

A Local Computer on the Space's own machine needs no code. A User starts the pairing there and confirms it. The Computer belongs to the User the ticket names: the Space Owner for a startup ticket, or the signed-in User who asked for one; [design](design.md#codes) covers the ticket.

A Local Computer on another machine prints a pairing code. A signed-in User enters that code in an app or a browser to approve the machine, and the Computer belongs to that User.

A Bot has no account. The Space gives a Bot its own credential when the Bot is created, and the Bot's loop uses it to act as the Bot.

## What this does not do yet

- Removing a User from a Space, demoting one as removal, or leaving a Space.
- Listing a User's signed-in devices, signing one out, or signing out everywhere.
- Changing a forgotten password, or resetting another User's account.
- Deleting a Space.
