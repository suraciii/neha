# Plugin

## Why

A user wants their writer to reach the company wiki. Another wants a Bot that runs on codex instead of Neha's own loop. A third wants a Group mirrored into Slack.

None of these change what Neha is. Each is a Plugin: something added that replaces or extends one part, with the rest untouched.

## What

A Plugin is the unit of replaceability: every part of Neha — model, tools, a Bot's loop, a Group — is a Plugin, a Plugin replaces any part or adds to it, and the Kernel they attach to is fixed.

Plugins are installed and enabled on a Space; a Bot uses the tools and loop its Plugins provide.

An integration with another service is a Plugin, whether it connects a tool, mirrors a Group elsewhere, or brings an outside agent in.

A Plugin never holds the record: the record is the Space's, and a Plugin reads and writes it like any other member.

## Why this way

One unit of replaceability keeps the product honest about ownership: nothing is a black box the user cannot swap.

Integrations as Plugins keeps Neha the entry point; a bridge to another surface is something added, never a second place where the record lives.

Keeping the record out of Plugins means a Plugin can come and go without taking history with it.

## Principles

- **Everything is a Plugin.** Model, tools, a Bot's loop, and a Group are Plugins; a Plugin replaces any part or adds to it. The Kernel is fixed.
- **Installed on the Space.** Plugins are installed and enabled on a Space, and a Bot uses what they provide.
- **Integrations are Plugins.** A bridge to another service is added to Neha; the record stays in Neha.
- **Plugins hold no record.** A Plugin reads and writes the record like a member and takes nothing with it when removed.

## What this enables

- A Bot that reaches the tools its owner already uses.
- A Bot on an outside harness, with its work landing in the Chat like any other Bot's.
- A Group mirrored elsewhere without a second record.

## User stories

- [S5 · Hand a repo to another harness](../USER-STORIES.md#s5--hand-a-repo-to-another-harness)
