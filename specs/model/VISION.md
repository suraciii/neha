# Model

## Why

A user signs up for an API key, pastes it, and their Bot works. Another runs their own OpenAI-compatible server, pastes its address, and picks a model from the list. A third gives their writer a cheap model and their researcher a strong one with high thinking.

A Bot thinks with a model, and Neha provides none. Connecting a provider has to take one paste, and choosing a model has to be possible per Bot, because the cheap and the strong both have a place.

## What

A Provider is a source of Models: a known vendor reached with an API key, or an OpenAI-compatible server reached with its address.

The Admin of a Space connects its Providers. Every Bot on the Space can use their Models.

A Model has capabilities: whether it reads images, whether it thinks, and its context limit.

The Space has a default Model. A Bot can choose its own Model and thinking level.

## Why this way

Neha provides no model because the model is the user's choice and the user's cost; a Space that runs on the user's own infrastructure takes the user's own keys.

An OpenAI-compatible address is the one universal case: whatever a user runs, it speaks it, so nothing else needs a special path.

Per-Provider protocol and headers live under Advanced because most users never need them and the few who do cannot work without them.

Capabilities are recorded per Model because a Bot that works a screen needs a model that reads images, and the picker has to say so before the work fails.

A Space default with a per-Bot choice keeps the floor flat and the ceiling high: most Bots never think about it, and the ones that need to can.

## Principles

- **Neha provides no model.** A Space thinks with the Models its Admin connects.
- **One paste to connect.** A known vendor takes an API key; a self-hosted server takes an OpenAI-compatible address, and its Models are found on their own.
- **Every Model states what it can do.** Images, thinking, and context limit are known before a Bot is given work.
- **A Space default, a Bot's choice.** Every Bot uses the Space's default Model unless its owner chooses another and a thinking level.

## What this enables

- A user with a key and nothing else.
- A user with their own server and no key at all.
- A cheap model for a writer and a strong one for a researcher, on the same Space.

## User stories

- [S1 · Tell the Bot, and it gets done](../USER-STORIES.md#s1--tell-the-bot-and-it-gets-done)
