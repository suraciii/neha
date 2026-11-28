# Identity

[product.md](product.md) states how a User signs in and how a device joins. This page states what every Space requires of a connection.

## Authority

Owned by identity. [CLI](../cli/product.md) and [Computer](../computer/product.md) join through this topic, and every client signs in through it.

## Laws

1. A Space admits a connection only from a caller it has signed in, a device that is pairing, or a Local Computer presenting a valid local ticket.
2. A connection to a Space is encrypted, except one from the Space's own machine or from a proxy the Space trusts.
3. A code works once, expires, and belongs to one caller and one Space. A code that a User types can be tried only a limited number of times.
4. A device on another machine validates the Space's certificate against the system trust store before it connects; without a public certificate it checks a key it received from a signed-in User.
5. The Space stores only a hash of a token, never its plaintext.
6. A client that loads the Space's own pages keeps its token only in an HttpOnly cookie. Any other client sends its token in the Authorization header.

## Local Space and Hosted Space

Every Space holds Accounts and signs Users in. Where a Space runs changes only its defaults.

A Local Space listens on its own machine until its Admin opens it to the network. A Hosted Space listens on its server from its first start.

## Signing in without a prompt

The app on the machine that runs a Space signs its first User in without a prompt. It proves itself with a secret that the app creates for one launch and hands to the Space's process over a pipe. The Space honors it for one sign-in request and never writes it to a file.

## A Space without a domain

A Space with a domain uses a public certificate. A Space without one uses a certificate of its own, and each link it gives out carries that certificate's key.

A device that receives a link from a signed-in User trusts the certificate the link names and no other. When the certificate changes, the device asks again.

## Codes

An invitation and a pairing code each admit one caller to one Space, work once, and expire. A pairing code is typed, so a User gets only a limited number of tries at it. The Space keeps a pairing code only as a hash, and a paired machine keeps the token that admits it.

A Local Computer started on the Space's own machine uses no code. The User who pairs the machine confirms the pairing there.

A Local Computer started on another machine uses a pairing code. The machine shows the code, and a signed-in User enters it in an app or a browser. The code admits that User and one Space.

A pairing code is eight letters shown as `XXXX-XXXX`, and it expires after fifteen minutes. The machine polls the Space for its answer; an approved code records the Computer, and an expired or replaced code answers no poll.

The Space bounds how many pairings wait at once and how many one source address MAY start, and refuses a start when a bound is reached.

A local ticket is offered only on the Space's own machine: its route answers a direct loopback request with no forwarded header, so a public origin does not expose it.

The Space issues a short-lived, one-use ticket that binds one User, one Space, and one Space start. Its database keeps only the ticket's digest, and a used or expired ticket admits no one.

A Space that starts with an Admin issues a ticket for that Admin, even with no signed-in client, as one file per Space under the machine's Neha configuration directory, with mode 0600. A Bot with granted folders cannot read it; [Computer](../computer/product.md#what-a-bot-can-do) states what full access reaches.

A signed-in User MAY request a ticket from the Space, and it stays in the answer.

The ticket's User owns the Computer it pairs. A process that can read the startup ticket MAY pair the machine as its Admin, and a signed-in caller MAY pair as itself.
