# Model

[product.md](product.md) states what a User connects and chooses. This page states what the Space requires of every provider adapter.

## Authority

Owned by model. It depends on [Space](../space/product.md) for the Providers a Space holds and on [Plugin](../plugin/product.md) for the adapter seam.

## Laws

1. Only the Space calls a Model, and a Provider's credential never reaches a Computer.
2. An adapter translates between one provider protocol and the shared vocabulary. Retry and compaction stay outside it.
3. A failed step is retried from its start. A stream the Model has begun is never resumed, and its partial output enters no context.
4. Every run records the usage a Provider reports, in one normalized form.

These Laws bind the built-in loop. A Bot whose loop is an outside harness waits for [Bot](../bot/design.md).

## Protocols

An adapter exists for OpenAI Chat Completions, for OpenAI Responses, and for Anthropic Messages. An OpenAI-compatible Provider is given the protocol it speaks, Chat Completions unless the Admin names another, and an adapter applies its vendor's dialect.

## Models

A Model's capabilities and limits come from a bundled snapshot of public Model data unless the Provider's configuration states them. A Model outside the snapshot states its context limit and capabilities in configuration.
