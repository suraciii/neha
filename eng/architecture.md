# Architecture

This page states the shape of Neha: what runs where, how the parts talk, and where each rule is enforced. [specs/](../specs/) states behavior and boundaries; [Ownership](#ownership) states which one wins.

## Deliverables and processes

Neha is one Rust codebase assembled by deliverable. The Space is the server: it holds the record and is its only writer. Everything else is a client of the Space or an executor for a Bot.

| Deliverable | Processes | How it reaches the Space |
| --- | --- | --- |
| Server, `neha serve` | The Space | Listens on the network from its first start |
| Desktop app | An Electron shell, a `neha serve` child, and a `neha computer` child once the User turns this machine on | The window loads a Space's address, its own or a remote one; closing the window hides the app, and the built-in Space keeps running unless the User set the app to exit |
| Web | None | Any browser; the Space serves the complete client |
| Android app | A Capacitor shell | Connects to a remote Space; the same UI runs in the system WebView |
| CLI, `neha <command>` | A client | The command surface |
| Real Computer | `neha computer`, a resident process | An outbound connection it initiates |
| Hosted Space | The Space, run by Neha | The same as a server |

```text
  CLI, Web, desktop window, Android
                 │  one WebSocket each, files over HTTP
                 ▼
   ┌──────────────────────────────────────────┐      Model providers
   │ Space: command surface, record, plugins, │ ──►  one adapter per protocol
   │ approval gate, SQLite                    │
   └──────────────────────────────────────────┘
        │                     ▲
        │ bytes of Files      │ outbound HTTPS,
        ▼                     │ one WebSocket per screen
   local directory or         neha computer on a real machine
   one S3 bucket prefix
```

The built-in Bot loop runs inside the Space and acts as a Bot through the command surface. [Bot](../specs/bot/design.md) states where work runs.

A built-in Space signs its first User in through a secret that the desktop shell creates for one launch and hands to the `neha serve` child over a pipe. A Space on a server prints an invitation link for its first Admin. [Identity](../specs/identity/product.md) covers both.

## Three planes

Neha's clients, its Computers, and Model providers talk to the Space on three planes that never share a connection.

### Command surface

The command surface carries typed commands and events between the Space and each client, GUI, CLI, or Bot, over one WebSocket; files travel over HTTP. [CLI](../specs/cli/design.md) states the Laws, including versioning and capability negotiation.

### Computer channel

A Computer connects out and listens on no network port. Control and data share one HTTPS connection, and each screen is its own outbound WebSocket. [Computer](../specs/computer/design.md) states the Laws.

### Model access

The built-in loop's Model calls leave from the Space, never from a Computer. One thin adapter serves each protocol, OpenAI Chat Completions, OpenAI Responses, or Anthropic Messages, and a provider-neutral vocabulary sits between the adapters and the Bot loop. [Model](../specs/model/design.md) states the Laws.

## Record and storage

SQLite is the source of truth. The record is an append-only log plus projections, and each Space has one local home directory. File bytes live in a store the operator configures: a directory in that home, or one prefix of an S3-compatible bucket. [Space](../specs/space/design.md) states the invariants.

## Plugin kernel

The [Kernel](../CONTEXT.md) is fixed. Everything else registers on a seam, which is one Rust trait and a registry, and a Plugin is built in, declarative, or out of process. [Plugin](../specs/plugin/design.md) states the seams and the kinds.

## Where authority is enforced

Two gates govern a Bot's actions: the Computer's owner grants what it can do, and the User it acts for approves each action that needs an approval. [Bot](../specs/bot/design.md) states which text is an instruction and which is data. [Space](../specs/space/design.md) states what the Space checks before it authenticates a request.

## Repository layout

```text
crates/
  neha/           domain, record, and command-surface types; no I/O
  neha-loop/      the built-in Bot loop
  neha-space/     the server: HTTP, command surface, storage
  neha-computer/  the resident Computer
  neha-app/       the `neha` binary: serve, computer, and the CLI subcommands
apps/
  desktop/        the Electron shell, outside the core workspace
ui/               the Solid client shared by Web, desktop, and Android, outside the core workspace
docs/             user-facing guidance
eng/              engineering practices
scripts/          verification tools
specs/            product contracts
```

The Android shell sits outside the core workspace as well. The repository root is a virtual manifest: the root `Cargo.toml` declares the members and the shared settings, and each crate lives under `crates/`. `just ci` covers the core workspace only.

## Adding a crate

A crate owns one capability. Add one when a capability needs a dependency set, a failure domain, or a build target that an existing crate cannot own. A line count does not justify a crate.

Each crate inherits `version`, `edition`, `rust-version`, `license`, and `publish` from `[workspace.package]`, and enables the shared lints with `[lints] workspace = true`.

Shared dependency versions are declared once in the root `[workspace.dependencies]` table. A crate then writes `foo.workspace = true`.

## Dependencies and toolchain

- Rust, pinned by [`rust-toolchain.toml`](../rust-toolchain.toml); the MSRV is 1.94; edition 2024.
- Every dependency's license must be compatible with GPL-3.0-only.
- A configuration file or manifest Neha owns is TOML. A file a standard defines keeps that standard's format.

## Cross-cutting conventions

- Every long-lived connection carries an application-level heartbeat, because proxies drop idle connections.
- Diagnostics do not leave the Space's machine by default.
- Design scale is one machine, one process, and hundreds of Users and Bots.
- An upgrade runs versioned migrations after an automatic backup and promises an upgrade from the previous release only.
- A backup copies the Space home and the objects of its File store, starting from a consistent database snapshot. A restore on another machine also takes the Vault's master key.

## Where a rule lives

| Topic | Behavior | Boundaries and invariants |
| --- | --- | --- |
| Space | [product](../specs/space/product.md) | [design](../specs/space/design.md) |
| Bot | [product](../specs/bot/product.md) | [design](../specs/bot/design.md) |
| Computer | [product](../specs/computer/product.md) | [design](../specs/computer/design.md) |
| Identity | [product](../specs/identity/product.md) | [design](../specs/identity/design.md) |
| Plugin | [product](../specs/plugin/product.md) | [design](../specs/plugin/design.md) |
| Model | [product](../specs/model/product.md) | [design](../specs/model/design.md) |
| CLI | [product](../specs/cli/product.md) | [design](../specs/cli/design.md) |
| Board | [product](../specs/board/product.md) | |
| Messaging | [chat](../specs/messaging/chat/product.md), [group](../specs/messaging/group/product.md), [message](../specs/messaging/message/product.md), [notification](../specs/messaging/notification/product.md), [search](../specs/messaging/search/product.md) | |
| Client | [product](../specs/client/product.md) | [design](../specs/client/design.md) |

## Ownership

One module owns each rule. An adapter may translate between models, but a second copy of a rule is a defect.

This page records boundaries. `specs/` records behavior. When the two disagree, the specification is authoritative, and this page is corrected in the same change.

## See also

- [Documentation language](documentation-language.md).
- [Test architecture](testing.md).
- [Product contracts](../specs/).
