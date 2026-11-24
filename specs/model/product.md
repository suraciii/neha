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
| A known vendor | Picks the vendor and pastes an API key | Lists the vendor's Models |
| An OpenAI-compatible server | Pastes the server's address | Finds the server's Models on its own |

Under Advanced, an OpenAI-compatible connection names its protocol: chat completions, which is the default, OpenAI Responses, or Anthropic Messages. Custom headers can be set there too.

The Admin can disconnect a Provider. Bots that used its Models fall back to the Space's default.

## Models

Each Model has a name, its Provider, and three capabilities: reads images, thinks, and context limit. The Admin can correct the capabilities of a Model from a self-hosted server.

A Model that cannot read images is marked in every picker, and a Bot that works a screen cannot choose it.

## Choosing

The Space has a default Model and thinking level. Every Bot uses them unless its owner chooses a Model and a thinking level for that Bot on its page.

| Thinking level | What it means |
| --- | --- |
| Minimal | Fastest and cheapest |
| Low, Medium, High | More thought, more time, more cost |
| Max | The most thought the Model offers |

## Failure

When a Provider fails or runs out, the Bot says so as a Message and the work is done.

## Gaps

- Signing in with a subscription plan instead of an API key is not yet specified.
- Fallback Models, used when the chosen Model fails, are not yet specified.
- A Model per Schedule or Watcher is not yet specified.
- Usage and cost by Model are not yet specified.
