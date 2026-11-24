# Plugin

## Authority

Owned by plugin. Every part of Neha is a Plugin, so every topic depends on this one for what it can be replaced with.

## Laws

1. Every part of Neha is a Plugin.
2. A built-in Plugin ships with Neha and is available on every Space.
3. A Plugin holds no record.

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

## Gaps

- Plugins a User installs, and who installs them, wait for the technical design.
- A Bot whose loop is an outside harness is not yet specified.
- Whether a skill is a Plugin or part of a Bot's memory is not yet specified.
