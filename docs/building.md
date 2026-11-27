# Building and verification

## Requirements

- Rust, pinned by [`rust-toolchain.toml`](../rust-toolchain.toml); the MSRV is 1.94.
- `just` for the recipes in [Justfile](../Justfile).
- `cargo-deny` for the dependency policy check, installed with `cargo install cargo-deny --locked`.
- Python 3 for the documentation and hygiene checks in `scripts/`.
- Node 24 and pnpm 11 for the browser client under `ui/`.
- Docker, for the local S3 test store.
- `bubblewrap` on Linux, for the sandbox that runs a Computer's shell commands, Local or Hosted.

The workspace has no system-library dependencies. SQLite and aws-lc-sys compile C code that ships inside the crates, so a build needs a C compiler; aws-lc-sys also uses cmake for some targets and build configurations. The browser client's dependencies are installed from its committed lockfile.

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
just clippy       # lints; warnings fail
just test         # workspace tests
just deny         # licenses, advisories, duplicate versions, registry sources
just test-s3      # neha-space tests with the S3 tests switched on
just doc          # documentation build; warnings fail
just check-specs  # links, headings, modal case, presentation
just checks       # the gate's fast half
just ci           # the full gate, including the browser client
just ui-deps      # install the browser client's locked dependencies
just ui-check     # type-check, test, and build the browser client
```

## S3 test store

The `neha-space` tests include S3 tests that need a local SeaweedFS container; [Storage](storage.md#run-the-s3-tests-locally) gives the store, the variables, and the command.

## A Computer sandbox

In granted mode a Local Computer runs shell commands in a `bubblewrap` sandbox on Linux; a Hosted Computer runs them in a minimal one on the Space's machine. A Bot with full access starts no sandbox. The file tools check write paths themselves and never start a command. Install the package with the system's package manager:

```bash
sudo apt install bubblewrap      # Debian, Ubuntu
sudo dnf install bubblewrap      # Fedora
sudo pacman -S bubblewrap        # Arch Linux
```

The runtime's sandbox tests skip when the machine cannot start one. Set `NEHA_TEST_BWRAP=1` to make a missing sandbox fail them; CI sets it after installing the package.

## Evidence

`just verify <directory> [recipe]` runs the `ci` gate, or the named recipe, one recipe at a time and writes the source snapshot, tool versions, timings, and output to a new directory. The directory must be outside the repository.

A change that alters behavior is exercised through the real entry point, and the pull request records the scenario and result.
