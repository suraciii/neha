# Identity

[product.md](product.md) states how a User signs in and how a device joins. This page states what every Space requires of a connection.

## Authority

Owned by identity. [CLI](../cli/product.md) and [Computer](../computer/product.md) join through this topic, and every client signs in through it.

## Laws

1. A Space admits a connection only from a caller it has signed in, or from a device that is pairing.
2. A connection to a Space is encrypted, except one from the Space's own machine or from a proxy the Space trusts.
3. A code works once, expires, and belongs to one caller and one Space. A code that a User types can be tried only a limited number of times.
4. A device checks the Space's key against one it received from a signed-in User before it connects.
5. The Space stores only a hash of a token, never its plaintext.
6. A client that loads the Space's own pages keeps its token only in an HttpOnly cookie. Any other client sends its token in the Authorization header.

## The same Space, deployed two ways

Every Space holds Accounts and signs Users in. Where a Space runs changes only its defaults.

A Space on a user's own machine listens on that machine alone until its Admin opens it to the network. A server listens on the network from its first start.

## Signing in without a prompt

The app on the machine that runs a Space signs its first User in without a prompt. It proves itself with a secret that the app creates for one launch and hands to the Space's process over a pipe. The Space honors it for one sign-in request and never writes it to a file.

## A Space without a domain

A Space with a domain uses a public certificate. A Space without one uses a certificate of its own, and each link it gives out carries that certificate's key.

A device that receives a link from a signed-in User trusts the certificate the link names and no other. When the certificate changes, the device asks again.

## Codes

An invitation and a pairing code each admit one caller to one Space, work once, and expire. A pairing code is typed, so a User gets only a limited number of tries at it. The Space keeps a pairing code only as a hash, never in clear.
