# Computer

[product.md](product.md) states what a Computer is for a User. This page states how a Computer connects to the Space and what passes between them.

## Authority

Owned by computer. A Computer's channel depends on [Space](../space/product.md) for the connection it makes and on [Identity](../identity/product.md) for the pairing that admits it.

## Laws

1. A Computer connects to the Space; the Space never connects to a Computer.
2. A Computer offers only what its owner granted.
3. A credential from the Vault reaches a Computer only when a Bot uses it.
4. A tool a Bot uses runs on the Bot's Computer.
5. One actor controls a screen at a time.
6. A Computer's control and its data share one outbound connection, and its screen never shares it.
7. The end of the control stream to the Space ends every process the Computer started during it, except a process a Bot started in background mode.
8. A Bot's command runs on pipes with its output and error kept apart, and ending it ends its whole process tree.

## The channel

A real Computer runs Neha as a persistent process. The process holds one outgoing connection to the Space for control and data, and one more per screen; it listens on no network port.

The channel carries commands, file reads and writes, process streams, and the screen. A managed Computer uses the same channel on the Space's own machine.

## Online and offline

A Computer is online while its channel is up. The Space shows it as offline after its channel has been down for longer than a set time.

## Tools

A tool that runs as its own process runs on the Computer as well, with what the Computer was granted.

## The screen

The Space relays a Computer's screen to its clients. Watching a screen and taking control of it are separate grants.

Taking control gives the keyboard and mouse to the User and pauses every Bot on that screen. Each Bot's other work continues.

On a real Computer, input from its own keyboard or mouse that Neha did not inject gives control to the User. A Bot does not replay an action that this interrupted, and observes the screen again before it acts after the User is done.

## Credentials

Neha removes a credential from the Vault out of everything a Bot sends back.

On a real Computer, a Bot uses the machine's own logins.
