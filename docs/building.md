# Building and verification

## Requirements

- Rust stable, version 1.94 or newer.
- `just` for the recipes in [Justfile](../Justfile).
- Python 3 for the documentation and hygiene checks in `scripts/`.

The workspace has no native dependencies.

## Commands

```bash
cargo build --locked --workspace
cargo test --locked --workspace
```

`just` is the supported command surface:

```bash
just fmt          # formatting check
just check        # type check
just clippy       # lints; warnings fail
just test         # workspace tests
just doc          # documentation build; warnings fail
just check-specs  # links, headings, modal case, presentation
just ci           # the full gate
```

## Evidence

`just verify <directory>` runs the `ci` gate one recipe at a time and writes the source snapshot, tool versions, timings, and output to a new directory. The directory must be outside the repository.

A change that alters behavior is exercised through the real entry point, and the pull request records the scenario and result.
