# Building and verification

## Requirements

- Rust stable, version 1.94 or newer.
- `just` for the recipes in [Justfile](../Justfile).
- Python 3 for the documentation and hygiene checks in `scripts/`.
- Node 24 and pnpm 11 for the browser client under `ui/`.
- Docker, for the local S3 test store.
- `bubblewrap` on Linux, for the sandbox a real Computer runs Bot actions in.

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
just test-s3      # neha-space tests with the S3 tests switched on
just doc          # documentation build; warnings fail
just check-specs  # links, headings, modal case, presentation
just ci           # the full gate, including the browser client
just ui-deps      # install the browser client's locked dependencies
just ui-check     # type-check, test, and build the browser client
```

## S3 test store

The `neha-space` tests include S3 tests that need a local SeaweedFS container; [Storage](storage.md#run-the-s3-tests-locally) gives the container, the bucket, the variables, and the command.

## A real Computer sandbox

The machine-side process of a real Computer runs Bot actions in a `bubblewrap` sandbox on Linux. Install the package with the system's package manager:

```bash
sudo apt install bubblewrap      # Debian, Ubuntu
sudo dnf install bubblewrap      # Fedora
sudo pacman -S bubblewrap        # Arch Linux
```

The fence tests skip when they cannot start a sandbox. Set `NEHA_TEST_BWRAP=1` to make a missing sandbox fail them; CI sets it after installing the package.

## Evidence

`just verify <directory>` runs the `ci` gate one recipe at a time and writes the source snapshot, tool versions, timings, and output to a new directory. The directory must be outside the repository.

A change that alters behavior is exercised through the real entry point, and the pull request records the scenario and result.
