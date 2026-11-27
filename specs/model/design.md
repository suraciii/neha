# Model

[product.md](product.md) states what a User connects and chooses. This page states what the Space requires of every provider adapter.

## Authority

Owned by model. It depends on [Space](../space/product.md) for the Providers a Space holds and on [Plugin](../plugin/product.md) for the adapter seam.

## Laws

1. Only the Space calls a Model, and a Provider's credential never reaches a Computer.
2. An adapter translates between one provider protocol and the shared vocabulary. Retry and compaction stay outside it.
3. A failed step is retried from its start. A stream the Model has begun is never resumed, and its partial output enters no context.
4. Every run records the usage a Provider reports, in one normalized form.
5. A step MAY carry provider-native continuation state to the next step. It is opaque, bound to the protocol, Provider, and Model that produced it, and never enters the Chat or another Provider's call.

These Laws bind the built-in loop. A Bot whose loop is an outside harness waits for [Bot](../bot/design.md).

## Protocols

An adapter exists for OpenAI Chat Completions, for OpenAI Responses, and for Anthropic Messages. An OpenAI-compatible Provider is given the protocol it speaks, Chat Completions unless the Admin names another, and an adapter applies its vendor's dialect.

A thinking level maps to the protocol's own reasoning control. The adapter sends one only when the Model is marked as thinking; a Model that cannot think refuses a Bot that asks for a level.

The provider-native output a later tool step needs, such as a reasoning item or a signed thinking block, is kept per run and step beside the reply. Recovery uses it when it is still available and otherwise continues without it.

## Models

A Model's capabilities and limits come from the catalog unless the Provider's configuration states them. The Provider record stores the resolved values, so a run reads one source: its Model's record. Manual values win over the catalog.

The catalog's `attachment` flag says a file can be attached, not that the Model reads images. Image input comes only from an `image` entry in the catalog's input modalities; a Model with no modalities reads no images.

A Model that cannot call tools never enters the loop. The loop's tool schemas are its only way to act, so a text-only Model is refused before a step starts.

## The default Model

The record names one default Model for the whole Space, and an event that names one clears the mark on every other Model.

The Admin's choice arrives as a `neha.provider.models_updated` event on the chosen Model's Provider, which re-states that Provider's Models and names the default one. The event changes no thinking level; the Space's default level comes from a Provider connection.

## Catalog

The catalog is a versioned data file, not a record event. A build embeds a full models.dev snapshot, and a Space reads the cached copy under its home when one is usable. A failed fetch leaves the last known good catalog in place.

The catalog maps a vendor's `npm` package to one protocol: OpenAI to Responses, Anthropic to Messages, and OpenAI-compatible to Chat Completions. A vendor with another package is visible but not connectable.

A fetched catalog is written atomically beside its ETag and timestamps. A cache that is corrupt, has another version, or lacks Providers or Models is ignored in favor of the snapshot. Background refresh runs only for an existing cache older than the configured interval.
