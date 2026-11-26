# Building and verification

## Requirements

- Rust stable, version 1.94 or newer.
- `just` for the recipes in [Justfile](../Justfile).
- Python 3 for the documentation and hygiene checks in `scripts/`.
- Node 24 and pnpm 11 for the browser client under `ui/`.

The Rust workspace has no native dependencies. The browser client's dependencies are installed from its committed lockfile.

## Commands

```bash
cargo build --locked --workspace
cargo test --locked --workspace
pnpm --dir ui install --frozen-lockfile
pnpm --dir ui build
```

`just` is the supported command surface:

```bash
just fmt          # formatting check
just check        # type check
just clippy       # lints; warnings fail
just test         # workspace tests
just doc          # documentation build; warnings fail
just check-specs  # links, headings, modal case, presentation
just ci           # the full gate, including the browser client
just ui-deps      # install the browser client's locked dependencies
just ui-check     # type-check, test, and build the browser client
```

## Evidence

`just verify <directory>` runs the `ci` gate one recipe at a time and writes the source snapshot, tool versions, timings, and output to a new directory. The directory must be outside the repository.

A change that alters behavior is exercised through the real entry point, and the pull request records the scenario and result.
