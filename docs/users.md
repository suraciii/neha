# Share a Space with other people

A Space can serve one person or a group of people. This guide shows the three roles in a Space, how an Admin invites someone, and how the Space Owner changes who administers it. It follows [Quick start](quickstart.md), so the Space, its Provider, and a Bot already exist.

## Roles

Every Space has one Space Owner, any number of Admins, and every other User is a Member. The Space Owner is an Admin too.

| Role | What the person can do |
| --- | --- |
| Space Owner | Everything an Admin does, plus make an Admin, change any other User's role, and transfer the Space. |
| Admin | Invite Users, connect Providers, install Plugins, change the Space's settings, and revoke invitations. |
| Member | Use the Space, read Settings, and own their Bots, Chats, and Computers. |

The first User of a Space becomes its Space Owner.

## See who is in the Space

Every signed-in User can read the People list in Settings, or run `neha user list`, and see each User's role.

## Invite someone

An Admin creates an invitation in Settings → People. The browser shows the link and its QR code once, when the invitation is made. Share the link or the QR code with the person you invite.

You can also create the invitation from the CLI:

```bash
target/debug/neha invitation create
```

The command prints the link once. The invitation admits one new User as a Member and expires after seven days. Making a new link replaces every earlier one, including one whose join failed.

List the pending invitations with their expiry, or revoke one before it is used:

```bash
target/debug/neha invitation list
target/debug/neha invitation revoke <invitation-id>
```

The invited person opens the link in an app or a browser, sets a display name and a password, and joins as a Member. A used or revoked invitation admits no one.

A Local Space's link works only from a device that can reach the machine that runs the Space.

## Make someone an Admin

Only the Space Owner changes a role. In Settings → People, the Space Owner sets another User's role, or runs:

```bash
target/debug/neha user list
target/debug/neha user role <name> admin
```

The same command with `member` makes that User a Member again. The command takes a display name or an identifier. The Space Owner cannot be demoted or removed.

## Transfer the Space

Only the Space Owner transfers the Space. The Space has one Space Owner at a time. In Settings → People, the Space Owner names the new Space Owner, or runs:

```bash
target/debug/neha user transfer <name>
# or, when the name is ambiguous:
target/debug/neha user transfer --id <user-id>
```

The receiving User becomes the Space Owner, and the sending User becomes an Admin.

## What this does not do yet

- Removing a User from a Space, or leaving one.
- Resetting a forgotten password.
- Sharing a Bot with another User.
- Managing devices and sessions, signing out, and changing your own password.
- Deleting the Space.
