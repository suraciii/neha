# Neha

A Rust workspace. [specs/VISION.md](specs/VISION.md) defines the product vision, and [CONTEXT.md](CONTEXT.md) defines its language.

This repository holds the product contracts, the engineering design, and the Rust workspace that implements the first milestone.

## Layout

| Path | Contents |
| --- | --- |
| `crates/` | Workspace crates: `neha` (domain and wire types), `neha-space` (the Space), `neha-app` (the `neha` CLI) |
| `specs/` | Product contracts: observable behavior, boundaries, invariants |
| `docs/` | User-facing guidance |
| `eng/` | Engineering practices |
| `scripts/` | Verification tools |
| `.github/` | CI workflow and contribution templates |

The target layout, including the shells and the UI, is in [Architecture](eng/architecture.md).

## Build

Rust stable (1.94 or newer), [`just`](https://github.com/casey/just), and Python 3 for the documentation and hygiene checks.

```bash
cargo build --locked --workspace
just ci
```

`just verify <directory>` runs the same gate and retains its evidence outside the repository.

## Quick start

[Quick start: chat with a Bot](docs/quickstart.md) runs a Space, joins as the Admin, connects a Provider, and chats with a Bot.

## Documentation

[specs/](specs/) holds the product contracts. [CONTEXT.md](CONTEXT.md) defines the domain language. [docs/README.md](docs/README.md) lists the guides. [Architecture](eng/architecture.md), [testing](eng/testing.md), [documentation language](eng/documentation-language.md), and [coding standards](CODING_STANDARDS.md) cover engineering practices.

## License

Neha is licensed under the [GNU General Public License v3.0](LICENSE), identified as `GPL-3.0-only`.
