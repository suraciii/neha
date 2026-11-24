# Identity

## Authority

Owned by identity. [Space](../space/product.md) holds the accounts; [CLI](../cli/product.md) and [Computer](../computer/product.md) join through it.

## Laws

1. Every User of a remote Space has an account on it.
2. The built-in Space signs in no one.
3. A device joins a Space only through a code that a signed-in User enters.

## Accounts

An account has a display name, a picture, and one or more ways to sign in. An email address is optional and never the account's identity.

The first account on a remote Space is its Admin, created with a setup code that the Space prints when it starts.

## Signing in

| Way | When the Space offers it |
| --- | --- |
| Passkey | The Space has a domain and HTTPS |
| Password | Every remote Space |
| An external provider | An Admin has added one as a Plugin |

A signed-in client stays signed in until the User signs out or the Admin removes the account.

## Inviting

The Admin makes an invitation link, which admits one User and expires.

The invited User opens the link in an app or a browser, sets a display name and a way to sign in, and joins the Space.

## Devices

The CLI signing in and a real Computer pairing both show a code.

A signed-in User enters the code in an app or a browser. The device joins the Space on that User's behalf: the CLI acts as the User, and the Computer belongs to the User.

A Bot has no account. The Space gives a Bot its own credential when the Bot is created, and the Bot's loop uses it to act as the Bot.

## Removing

The Admin removes a User, and a User can leave on their own. Either way the account stops signing in, devices that joined through it lose access, and the User's Bots and Computers stay until the Admin deletes them.

## Recovering an account

When a User loses their way to sign in, the Admin resets the account. The User signs in with a temporary password and sets a new way to sign in.
