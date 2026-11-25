# Plugin

## Authority

Owned by plugin. Every part of Neha is a Plugin, so every topic depends on this one for what it can be replaced with.

## Laws

1. Every part of Neha outside the Kernel is a Plugin.
2. A built-in Plugin ships with Neha and is available on every Space.
3. A Plugin holds no record.
4. The Kernel is fixed, and a Plugin attaches to it.

## Built-in Plugins

Neha ships with its own Plugins: the model adapters, the tools, a Bot's loop, and the Group.

Every Bot on every Space can use the built-in Plugins. A built-in Plugin is replaced only by a new version of Neha.

## What a Bot gets from Plugins

A Bot's loop, its tools, and its model adapter each come from a Plugin.

## Integrations

An integration with another service is a Plugin. It connects a tool, mirrors a Group to another surface, or brings an outside agent in.

A bridge carries Messages between Neha and the other surface. The record stays in the Space; the other surface holds a copy.

## The record

A Plugin reads and writes the Space's record like a member. Removing a Plugin leaves the record as it was.

## Installing

The Admin installs and enables a Plugin on the Space. The owner of a real Computer approves any code a Plugin runs on that Computer.

A reference that starts a program on a Computer counts as code.

## Skills

A skill is a set of instructions for how to do a task. A skill adds no authority: its instructions carry only what the Bot was already allowed to do.

A skill is content kept in the record, not a Plugin. A Plugin can carry skills, and installing it adds them to the Space's library or to a Bot's memory.

The Admin installs skills on the Space, and a Bot's owner enables them for the Bot. A skill that a Bot wrote itself is not installed; [Bot](../bot/product.md) covers it.

A script that a skill carries runs only as one of the Bot's actions, and the owner of a real Computer approves it before a Bot on that Computer runs it.

The Admin can set a Space to treat skills as instructions only; then the scripts of a skill never reach a Computer.

## Gaps

- A Bot whose loop is an outside harness is not yet specified.
