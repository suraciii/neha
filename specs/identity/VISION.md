# Identity

## Why

A friend opens a link, picks a name and a passkey, and is in. Another user runs Neha on a home server at an IP address and sets a password instead. A third pairs a laptop by typing a code into the app.

Identity has to disappear for a user alone, work on whatever the Space runs on, and never make the Admin configure email.

## What

A User's identity is an account on the Space: a display name and a way to sign in.

The built-in Space signs its first user in without a prompt; that user is its Admin.

A remote Space signs a User in with a passkey, with a password, or with an external provider added as a Plugin.

The Admin invites a User with a link. The invited User sets a name and a way to sign in, and joins.

A device joins by showing a code that a signed-in User enters in an app, except a Local Computer on the Space's own machine, which joins without one.

## Why this way

Accounts live on the Space because the Space is what the user owns; no outside service stands between a user and their Bots.

A passkey is offered when the Space has a domain and HTTPS, and a password is always offered, because a Space on a home network has neither and still has to be usable.

Invitations are links because email is the one thing a self-hoster does not want to configure.

A device shows a code because a terminal or a headless machine cannot receive a sign-in, and a user already signed in can vouch for it. A Local Computer on the Space's own machine needs no code because the User who pairs it starts the pairing there.

Sign-in methods are Plugins because a Space that already has an identity provider can use it, and one that does not sees nothing about it.

## Principles

- **Accounts live on the Space.** A User is known to a Space by an account on it, with a display name and a way to sign in.
- **No sign-in step when alone.** The built-in Space signs its first user in without a prompt, and that user is its Admin.
- **Passkey when the Space can, password always.** A Space with a domain and HTTPS offers a passkey; every remote Space offers a password.
- **Invite by link.** The Admin makes an invitation link; the invited User sets up their own account.
- **Devices join by code, except the Local Computer.** A CLI shows a code, and a signed-in User enters it; a Local Computer on the Space's own machine joins without one.
- **Sign-in methods are Plugins.** An external provider is added to a Space as a Plugin and shows up on its sign-in page.

## What this enables

- Using Neha alone with nothing to set up.
- A home server on a local network that anyone in the house can join.
- A Space that reuses the identity provider a household or company already runs.
- A CLI and headless machines that never ask for a password.

## User stories

- [S4 · A Bot on every machine I own](../USER-STORIES.md#s4--a-bot-on-every-machine-i-own)
