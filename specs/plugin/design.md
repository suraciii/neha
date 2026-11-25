# Plugin

[product.md](product.md) states what a Plugin is for a User. This page states how Neha is built from Plugins and what a Plugin can be.

## Authority

Owned by plugin. Every topic depends on this one for what a Plugin can replace.

## Laws

1. A Plugin contributes to Neha only by registering on a seam.
2. Every registration is reversible, and removing a Plugin removes all that it registered.
3. Code that is not built in runs in a process of its own, and that process cannot read the Space's files.
4. Neha loads no dynamic library and hot-reloads no Plugin.
5. An installed Plugin is pinned to one content digest, and to one commit when it comes from a repository.
6. Installing a Plugin never executes code from its source.
7. A Space installs a Plugin only from a source its Admin allows.

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
| Declarative | Data: a server to connect to, a prompt, a skill, a Card schema, or a bridge's configuration | Checked on install; runs no code |
| Out of process | A program that speaks a protocol to the Space or to a Computer | Runs in a process of its own |

A reference that starts a program on a Computer is code, and Neha treats it as such.

## Composition

A Space's composition is an ordered list of rows. A row names a Plugin, its provider, its configuration, and whether it is enabled.

A built-in Plugin is in every composition. A row replaces only the row of a Plugin that is not built in.

## Failures

A Plugin declares the versions of Neha it supports. Neha does not activate a Plugin outside that range and reports the row.

A Plugin that fails to activate does not prevent another Plugin from activating.
