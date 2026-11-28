# Model

## Authority

Owned by model. [Space](../space/product.md) holds the Providers; [Bot](../bot/product.md) chooses a Model; a model adapter is a [Plugin](../plugin/product.md).

## Laws

1. A Space has at least one Provider before any Bot can work.
2. A Bot uses exactly one Model at a time.
3. A Model's capabilities are known before a Bot is given work.

## Providers

An Admin connects a Provider to the Space in one of two ways.

| Way | What an Admin gives | What Neha does |
| --- | --- | --- |
| A known vendor | Picks the vendor and pastes an API key | Reads the vendor's Models and address from the catalog |
| An OpenAI-compatible server | Pastes the server's address | Finds the server's Models on its own |

A known vendor MUST resolve from the catalog to one of the protocols Neha implements: OpenAI Responses, Anthropic Messages, or Chat Completions. It MUST also name a base URL Neha can use and at least one Model that calls tools.

A vendor without those stays visible in the catalog but cannot be connected without an explicit address or a protocol Neha does not implement.

Under Advanced, a connection names its protocol: chat completions, which is the default, OpenAI Responses, or Anthropic Messages. It can also override the base address.

An Admin can refresh a Provider's Models from its catalog entry or its endpoint. A refresh keeps a Model's identity when its name stays, and retires Models the Provider no longer offers.

A fresh known-vendor connection defaults to a Model that calls tools.

## Catalog

Neha ships a complete models.dev catalog snapshot. It names every vendor, whether Neha can speak its protocol, and each Model's limits and capabilities.

The catalog is data, not a record: it never enters the Chats, the Messages, or a Bot's context. A Space caches the catalog under its home and MAY refresh it in the background or on an explicit refresh.

A refresh uses the cached ETag, keeps the last known good catalog when the network or the answer fails, and never blocks the Space's start. A fresh offline Space uses the built-in snapshot.

## Models

Each Model has a name, its Provider, and these capabilities: context window, largest output, reads images, calls tools, and thinks. The catalog states them for a known vendor; an Admin states them for a manual Model.

The catalog's image capability comes from its input modalities: a Model reads images only when `image` is among them. File attachment support alone does not make a Model an image reader.

A Model's thinking levels come from the catalog's reasoning options. A Model that cannot think has no level; a Model that can think accepts the levels named for it, or every level when none are named.

An Admin can correct a Model's capabilities from a self-hosted server. Manual values win over the catalog.

A Model that cannot read images is marked in every picker, and a Bot that works a screen cannot choose it. A Model that cannot call tools refuses a run.

## Choosing

The Space has a default Model and thinking level. Every Bot uses them unless its owner chooses a Model and a thinking level for that Bot.

An Admin changes the default Model in [Settings](../client/product.md#settings) or with [`neha model default`](../cli/product.md#providers). Each Provider's default Model is the Space's default Model, and a Provider that does not offer it marks no default.

Changing the default Model changes no thinking level; the Space's default level comes from a Provider connection.

The new default takes over every Bot that inherits it. The Space refuses a Model that cannot serve a level such a Bot keeps, and the refusal counts those Bots and asks for their levels to be reset first. A Bot with a Model of its own is untouched.

Connecting or refreshing a Provider can move the default Model too, and the Space refuses the whole write when the Model it would set leaves a Bot unable to run, so the Space keeps the default it has.

A refresh judges every Bot that runs on a Model of the Provider it refreshes, on the Models it would leave behind. A Bot whose own Model retires inherits the refreshed default and keeps its thinking level, even when the default does not move.

A Model that keeps its name but changes what it serves is judged by what it now serves.

When a judged Bot could no longer run, or the refreshed default cannot call tools, the Space refuses the whole refresh and records nothing: the Models and the default the Space had stay as they were.

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
- Disconnecting a Provider is not yet available. When it is, Bots that used its Models fall back to the Space's default and keep their own thinking levels.
