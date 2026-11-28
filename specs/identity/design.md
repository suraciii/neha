# Identity

[product.md](product.md) states the roles, how a User signs in, and how a device joins. This page states what every Space requires of a connection.

## Authority

Owned by identity. [CLI](../cli/product.md) and [Computer](../computer/product.md) join through this topic, and every client signs in through it.

## Laws

1. A Space admits a connection only from a caller it has signed in, a device that is pairing, or a Local Computer presenting a valid local ticket.
2. A connection to a Space is encrypted, except one from the Space's own machine or from a proxy the Space trusts.
3. A code works once, expires, and belongs to one caller and one Space. A code that a User types can be tried only a limited number of times.
4. A device on another machine validates the Space's certificate against the system trust store before it connects; without a public certificate it checks a key it received from a signed-in User.
5. The Space stores only a hash of a token, never its plaintext.
6. A client that loads the Space's own pages keeps its token only in an HttpOnly cookie. Any other client sends its token in the Authorization header.
7. An invitation admits one User, and the Space stores only a hash of its secret.

## Local Space and Hosted Space

Every Space holds Accounts and signs Users in. Where a Space runs changes only its defaults.

A Local Space listens on its own machine until its Admin opens it to the network. A Hosted Space listens on its server from its first start.

Every Space offers invitations; the deployment mode changes only the reach of a Local Space's link.

## Signing in on the Space's own machine

The desktop app starts a Local Space and opens its invitation or sign-in page. It never creates the Space Owner or signs in for the User.

A Local Space's first User joins through the invitation page, in a browser or the CLI, exactly as a User on another machine does.

## Roles

The record gives an account its role: a role change is a `neha.user.updated` event and a transfer is a `neha.user.transferred` event. [Space](../space/design.md#user-events) states their data.

The Space records a role before it writes the account's role, and a startup sync reads every role again. A crash between the two therefore leaves no account out of step.

The record enforces one Space Owner on replay. A role change and the transfer serialize on one lock, so the Owner each checks and the event it writes cannot interleave with another such command.

## Invitations

An invitation admits one new User to a Space as a Member. The invitation lives in the Space, which stores only a hash of its secret and returns the secret once, in the live answer that creates it.

The stored answer names the invitation and omits the secret, so a retry after a crash recovers the invitation but not its link.

The invitation takes the identifier of the command that made it. The create or revoke writes the command's answer in the same immediate database transaction as the invitation change, so a crash leaves both or neither.

A retry therefore cannot rotate a newer link or report a revoke it did not just perform. A repeated create answers the same invitation and rotates nothing; a create with a new identifier replaces every earlier link, including a claimed one.

An invitation expires 7 days after it is made, and a used or revoked invitation admits no one. An Admin revokes a pending one.

Every create and revoke also appends `neha.invitation.changed`, so an Admin's open clients read the pending invitation again. The event names no invitation and carries no secret.

The transaction also records the event command that notification takes. A crash before the append leaves that notice behind, and the next start appends the same event and forgets the notice.

Joining claims the invitation before the Space writes the account and finishes after the record holds the join. A startup repair resolves claims first, then drops accounts the record never held, then refuses to serve when the record names a User no account signs in.

The repair completes a claim whose User the record holds, and releases one whose join never reached the record.

## A Space without a domain

A Space with a domain uses a public certificate. A Space without one uses a certificate of its own, and each link it gives out carries that certificate's key.

A device that receives a link from a signed-in User trusts the certificate the link names and no other. When the certificate changes, the device asks again.

## Codes

A pairing code admits one caller to one Space, works once, and expires. A pairing code is typed, so a User gets only a limited number of tries at it. The Space keeps a pairing code only as a hash, and a paired machine keeps the token that admits it.

A Local Computer started on the Space's own machine uses no code. The User who pairs the machine confirms the pairing there.

A Local Computer started on another machine uses a pairing code. The machine shows the code, and a signed-in User enters it in an app or a browser. The code admits that User and one Space.

A pairing code is eight letters shown as `XXXX-XXXX`, and it expires after fifteen minutes. The machine polls the Space for its answer; an approved code records the Computer, and an expired or replaced code answers no poll.

A CLI signing in without a session uses the same code shape and lifetime. The CLI generates its token and polling secret, the Space stores only their digests, and approving the code inserts the token hash as that User's ordinary session in the same transaction that claims the code.

A retry of the approving command answers the account already bound instead of minting a second session.

The Space bounds how many pairings wait at once and how many one source address MAY start, and refuses a start when a bound is reached.

A local ticket is offered only on the Space's own machine: its route answers a direct loopback request with no forwarded header, so a public origin does not expose it.

The Space issues a short-lived, one-use ticket that binds one User, one Space, and one Space start. Its database keeps only the ticket's digest, and a used or expired ticket admits no one.

A Space with a Space Owner issues a ticket for the Space Owner, even with no signed-in client, as one file per Space under the machine's Neha configuration directory, with mode 0600. A Bot with granted folders cannot read it; [Computer](../computer/product.md#what-a-bot-can-do) states what full access reaches.

A signed-in User MAY request a ticket from the Space, and it stays in the answer.

The ticket's User owns the Computer it pairs. A process that can read the startup ticket MAY pair the machine as the Space Owner, and a signed-in caller MAY pair as itself.
