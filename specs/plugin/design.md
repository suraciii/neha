# Plugin

[product.md](product.md) states what a Plugin is for a User. This page states how Neha is built from Plugins, what an installed Plugin brings, and what it never does.

## Authority

Owned by plugin. Every topic depends on this one for what a Plugin can replace.

## Laws

1. A Plugin contributes to Neha only by registering on a seam.
2. Every registration is reversible, and removing a Plugin removes all that it registered.
3. Code that is not built in runs in a process of its own, and that process cannot read the Space's files, except a server an Admin allowed on a Windows Space, which has no sandbox.
4. Neha loads no dynamic library and hot-reloads no Plugin.
5. An installed Plugin is pinned to one content digest, and to one commit when it comes from a repository.
6. Installing a Plugin never executes code from its source.
7. A Space installs a Plugin only from a source its Admin allows.
8. An installed Plugin writes to the record only through the command surface and only with the kinds the Kernel defines.
9. A Plugin keeps its state outside the record.
10. An MCP server's credential lives in the Vault and never reaches a Bot, the record, or a Computer.
11. A tool a Plugin brings meets the same [Approval](../bot/design.md#approvals) as a built-in tool.

## Seams

A seam is one replaceable part of Neha: a Rust trait and a registry of the providers that implement it.

| Seam | Providers | The topic that uses it |
| --- | --- | --- |
| Model adapter | Any number | [Model](../model/product.md) |
| Tool | Any number | [Bot](../bot/product.md) |
| Loop | Any number; a Bot uses one | [Bot](../bot/product.md) |
| Group | One | [Group](../messaging/group/product.md) |
| Sign-in method | Any number | [Identity](../identity/product.md) |
| Bridge | Any number | This topic |
| Card kind | Any number | [Message](../messaging/message/product.md) |

## Kinds of Plugin

| Kind | What it contains | How Neha uses it |
| --- | --- | --- |
| Built-in | Rust code shipped with Neha | Compiled in; a new version of Neha replaces it |
| Package | An Agent Plugins directory: skills, MCP servers, and Neha's client extension | Read on install; runs no code from the package |
| Server | A program that speaks the Model Context Protocol to the Space | Runs in a process of its own, or on another machine |

A reference that starts a program on a Computer is code, and Neha treats it as such.

## Package

An installed Plugin follows version 1.0.0 of the Agent Plugins specification. Neha reads `plugin.json`, `skills/`, `mcp.json`, and its own client extension under `ai.neha`, and ignores every other component.

Neha recognizes only that version: it validates the `$schema` of `plugin.json` and of `mcp.json` against the schemas it ships, and rejects any other version. It never fetches a schema.

Neha reads a skill as the Agent Skills specification defines it, and an MCP server with the `streamable-http` or the `stdio` transport.

Neha enforces a size limit on a package and rejects a package above it with a report; the help states the limit.

An invalid `plugin.json` rejects the package. An invalid skill, server entry, or unsupported transport is skipped and reported, and the rest of the package loads.

## Where server code runs

A `streamable-http` server runs elsewhere; the Space calls it over HTTPS, or over HTTP on loopback.

A `stdio` server runs on the Space's machine in a sandbox that mounts the system read-only and the server's data directory read-write. On Linux that sandbox is bubblewrap.

On Windows an Admin allows an unsandboxed server with `NEHA_ALLOW_UNSANDBOXED_MCP=1`. Without it, Neha skips the server and reports why.

## Tools

The Model sees a server's tool as `mcp__<server>__<tool>`. A server's name holds letters, digits, and `-` only, so the first `__` after the prefix ends it.

A server's name is unique in a Space. A package's server takes a name from the package's name and its key, and a name already taken skips that server with a report.

Neha replaces a character outside letters, digits, `_`, and `-` in a tool's name with `_`. When the replacement or the 64-character limit changes a name, Neha appends a hash of the server and the tool.

Neha forms the names of all a Bot's tools together, and a name that more than one tool would receive gets the hash appended to each.

The record, a Rule, and a call name a tool by its server's id and the tool's original name, never by the name the Model sees. A credential names its server and its scope, never a tool.

A tool's result enters the record as an action's result: text as text, an image as a File, and anything else as JSON text.

## Credentials

An MCP server's credential belongs to the Space or to one User. An Admin connects a server for the Space; a User connects it with their own credential.

A call finds the credential by the server's id: the acting User's when one exists, and otherwise the Space's. With neither, the tool returns that the server is not connected and the call sends no request; a `stdio` server's call launches without a credential instead, and a credential attached to it reaches the process as `NEHA_MCP_TOKEN`.

## Record

An installed Plugin reads and writes the record like a member and holds none of it. Its calls enter the record as actions, and a skill it brings enters as a Skill entry.

| Kind | What it records |
| --- | --- |
| `neha.mcp.server.added`, `neha.mcp.server.updated`, `neha.mcp.server.removed` | An MCP server, its scope, and the account that connected it |
| `neha.plugin.installed`, `neha.plugin.removed` | A package, its digest, its commit, and what it brought |
| `neha.plugin.enabled`, `neha.plugin.disabled` | Whether the Space offers a package |
| `neha.bot.mcp.enabled`, `neha.bot.mcp.disabled` | Whether a Bot can use a server |
| `neha.skill.added`, `neha.skill.removed` | A skill, its library, and the package that brought it when one did |
| `neha.bot.skill.enabled`, `neha.bot.skill.disabled` | Whether a Bot can use a skill |

Each is an Authority event. An installed Plugin declares no kind.

## Skills

A skill's name and description enter the Bot's context when the owner enables the skill; its body enters when the Bot loads it.

A script a skill carries runs as the `run_skill_script` action on the Computer. [Bot](../bot/design.md#instructions-and-data) states its Approval.

## Cards

A tool's result MAY carry an interactive Card of kind `app`. The client renders it in a frame that shares no origin with the Space, and the Space relays its messages to the server.

A Card of an unknown kind shows a placeholder and stops no reader.

## Composition

A Space's composition is an ordered list of rows. A row names a Plugin, its provider, its configuration, and whether it is enabled.

A built-in Plugin is in every composition. A row replaces only the row of a Plugin that is not built in.

## Failures

A Plugin declares the versions of Neha it supports. Neha does not activate a Plugin outside that range and reports the row.

A Plugin that fails to activate does not prevent another Plugin from activating.

A server that fails to connect or to answer leaves the other servers and the skills of its package usable.
