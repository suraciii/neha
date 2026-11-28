# Plugin

## Authority

Owned by plugin. Every part of Neha is a Plugin, so every topic depends on this one for what it can be replaced with.

## Laws

1. Every part of Neha outside the Kernel is a Plugin.
2. A built-in Plugin ships with Neha and is available on every Space.
3. A Plugin holds no record.
4. The Kernel is fixed, and a Plugin attaches to it.
5. A Plugin adds no authority: a tool it brings asks like a built-in tool, and a skill it carries grants nothing.

## Built-in Plugins

Neha ships with its own Plugins: the model adapters, the tools, a Bot's loop, and the Group.

Every Bot on every Space can use the built-in Plugins. A built-in Plugin is replaced only by a new version of Neha.

## What a Bot gets from Plugins

A Bot's loop, its tools, and its model adapter each come from a Plugin.

A Bot uses only the MCP servers and skills its owner enabled for it. A server a User added is offered to the Bot only while the Bot acts for that User.

## Integrations

An integration with another service is a Plugin. It connects a tool, mirrors a Group to another surface, or brings an outside agent in.

A bridge carries Messages between Neha and the other surface. The record stays in the Space; the other surface holds a copy. A bridge is built in; a package brings none.

## The record

A Plugin reads and writes the Space's record like a member. Removing a Plugin leaves the record as it was. [design](design.md#record) states what an installed Plugin can write.

## Adding tools directly

An Admin adds an MCP server for the Space under a name, and a User adds one for themselves or connects a Space's server with their own credential. The one who connects it supplies the credential: an access token, or a sign-in the browser completes.

The Bot never sees the credential. A server the Space cannot reach stays listed and shows as not connected; a Bot that calls its tool gets that answer and says so.

## Installing a package

An Admin installs a package from a source an Admin allows. The install pins the package's content and, for a repository, one commit, and shows what the package brings: its servers, its skills, and the parts it skipped.

Installing runs no code. Removing a package removes the servers and skills it brought, and leaves the record as it was.

## Code on a Computer

A reference that starts a program on a Computer counts as code. A skill's script is such a reference, and it runs as one of the Bot's actions under the Bot's approvals; a package brings no other.

## Cards

A tool MAY answer with an interactive Card in the Chat. The Card cannot read the User's session, and it shows the Space's theme and language.

## Skills

A skill adds no authority: its instructions carry only what the Bot was already allowed to do.

A package can carry skills; installing it creates their entries in the Space's library.

An Admin adds a skill to the Space's library; a Bot's owner adds one to that Bot's library and enables skills for the Bot. A skill that a Bot wrote itself is not installed; [Bot](../bot/product.md) covers it.

A script that a skill carries runs only as one of the Bot's actions, and its first run for the Bot and the acting User asks before it proceeds.

An Admin can set a Space to treat skills as instructions only; then the scripts of a skill never reach a Computer.

## Gaps

- A Bot whose loop is an outside harness is not yet specified.
- A catalog of packages to browse is not yet specified.
