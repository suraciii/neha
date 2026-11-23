# Neha

A Rust workspace. The product scope is not defined yet; [specs/](specs/) records it when it is, and [CONTEXT.md](CONTEXT.md) records its language.

This repository holds the product contracts and the engineering design; it contains no product behavior.

## Layout

| Path | Contents |
| --- | --- |
| `crates/` | Workspace crates; `crates/neha/` is the current library crate |
| `specs/` | Product contracts: observable behavior, boundaries, invariants |
| `docs/` | User-facing guidance |
| `eng/` | Engineering practices |
| `scripts/` | Verification tools |
| `.github/` | CI workflow and contribution templates |

## Build

Rust stable (1.94 or newer), [`just`](https://github.com/casey/just), and Python 3 for the documentation and hygiene checks.

```bash
cargo build --locked --workspace
just ci
```

`just verify <directory>` runs the same gate and retains its evidence outside the repository.

## Documentation

[specs/](specs/) holds the product contracts. [CONTEXT.md](CONTEXT.md) defines the domain language. [docs/README.md](docs/README.md) lists the guides. [Crate architecture](eng/architecture.md), [testing](eng/testing.md), [documentation language](eng/documentation-language.md), and [coding standards](CODING_STANDARDS.md) cover engineering practices.

## License

Neha is licensed under the [GNU General Public License v3.0](LICENSE), identified as `GPL-3.0-only`.
