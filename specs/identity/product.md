# Identity

## Authority

Owned by identity. [Space](../space/product.md) holds the accounts; [CLI](../cli/product.md) and [Computer](../computer/product.md) join through it.

## Laws

1. Every User of a Space has an account on it.
2. A Local Space's first User creates the Admin account through the invitation page; no client creates or signs in an account silently.
3. A device joins a Space through a code that a signed-in User enters. A Local Computer on the Space's own machine joins through a local ticket instead.

## Accounts

An account has a display name, a picture, and one or more ways to sign in. An email address is optional and never the account's identity.

The first account on a Hosted Space is its Admin, created with an invitation link that the Space prints when it starts.

## Signing in

| Way | When the Space offers it |
| --- | --- |
| Passkey | The Space has a domain and HTTPS |
| Password | Every Space reached over the network |
| An external provider | An Admin has added one as a Plugin |

A signed-in client stays signed in until the User signs out or removes that client, or the Admin removes the account.

A User can see the clients signed in as them and remove any one of them. A removed client is signed out at once.

## Inviting

The Admin makes an invitation link, which admits one User and expires. An invitation also shows as a QR code, which admits the same one User.

The invited User opens the link in an app or a browser, sets a display name and a way to sign in, and joins the Space.

## Devices

The CLI signing in shows a code.

A signed-in User enters the code in an app or a browser. The CLI joins the Space on that User's behalf and acts as the User.

A Local Computer on the Space's own machine needs no code. A User starts the pairing there and confirms it. The Computer belongs to the User the ticket names: the Admin for a startup ticket, or the signed-in User who asked for one; [design](design.md#codes) covers the ticket.

A Local Computer on another machine joins through a pairing code that a signed-in User enters on that machine. The Computer belongs to the User who made the code.

A Bot has no account. The Space gives a Bot its own credential when the Bot is created, and the Bot's loop uses it to act as the Bot.

## Removing

The Admin removes a User, and a User can leave on their own. Either way the account stops signing in, devices that joined through it lose access, and the User's Bots and Computers stay until the Admin deletes them.

## Recovering an account

When a User loses their way to sign in, the Admin resets the account. The User signs in with a temporary password and sets a new way to sign in.
