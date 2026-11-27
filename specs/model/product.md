# Model

## Authority

Owned by model. [Space](../space/product.md) holds the Providers; [Bot](../bot/product.md) chooses a Model; a model adapter is a [Plugin](../plugin/product.md).

## Laws

1. A Space has at least one Provider before any Bot can work.
2. A Bot uses exactly one Model at a time.
3. A Model's capabilities are known before a Bot is given work.

## Providers

The Admin connects a Provider to the Space in one of two ways.

| Way | What the Admin gives | What Neha does |
| --- | --- | --- |
| A known vendor | Picks the vendor and pastes an API key | Reads the vendor's Models and address from the catalog |
| An OpenAI-compatible server | Pastes the server's address | Finds the server's Models on its own |

A known vendor MUST resolve from the catalog to one of the protocols Neha implements: OpenAI Responses, Anthropic Messages, or Chat Completions. It MUST also name a base URL Neha can use and at least one Model that calls tools.

A vendor without those stays visible in the catalog but cannot be connected without an explicit address or a protocol Neha does not implement.

Under Advanced, a connection names its protocol: chat completions, which is the default, OpenAI Responses, or Anthropic Messages. It can also override the base address.

The Admin can refresh a Provider's Models from its catalog entry or its endpoint. A refresh keeps a Model's identity when its name stays, and retires Models the Provider no longer offers.

A fresh known-vendor connection defaults to a Model that calls tools. The Admin can disconnect a Provider; Bots that used its Models fall back to the Space's default.

## Catalog

Neha ships a complete models.dev catalog snapshot. It names every vendor, whether Neha can speak its protocol, and each Model's limits and capabilities.

The catalog is data, not a record: it never enters the Chats, the Messages, or a Bot's context. A Space caches the catalog under its home and MAY refresh it in the background or on an explicit refresh.

A refresh uses the cached ETag, keeps the last known good catalog when the network or the answer fails, and never blocks the Space's start. A fresh offline Space uses the built-in snapshot.

## Models

Each Model has a name, its Provider, and these capabilities: context window, largest output, reads images, calls tools, and thinks. The catalog states them for a known vendor; the Admin states them for a manual Model.

A Model's thinking levels come from the catalog's reasoning options. A Model that cannot think has no level; a Model that can think accepts the levels named for it, or every level when none are named.

The Admin can correct a Model's capabilities from a self-hosted server. Manual values win over the catalog.

A Model that cannot read images is marked in every picker, and a Bot that works a screen cannot choose it. A Model that cannot call tools refuses a run.

## Choosing

The Space has a default Model and thinking level. Every Bot uses them unless its owner chooses a Model and a thinking level for that Bot.

A Bot's chosen Model MUST be connected and able to call tools. Its thinking level MUST be one the Model accepts.

| Thinking level | What it means |
| --- | --- |
| Minimal | Fastest and cheapest |
| Low, Medium, High | More thought, more time, more cost |
| Max | The most thought the Model offers |

A Bot's level applies to every step of its runs. A Model marked as not thinking refuses a Bot that asks for a level. A Bot without its own level uses the Space's default, or the closest level its Model accepts.

## Failure

When a Provider fails or runs out, the Bot says so as a Message and the work is done.

## Gaps

- Signing in with a subscription plan instead of an API key is not yet specified.
- Fallback Models, used when the chosen Model fails, are not yet specified.
- Cost by Model is not yet specified.
