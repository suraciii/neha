set dotenv-load := false

# Show the available project commands when no recipe is supplied.
default:
    @just --list

# Check formatting without modifying files.
fmt:
    cargo fmt --all -- --check

# Type-check every workspace target.
check:
    cargo check --locked --workspace --all-targets

# Lint the workspace; any warning fails.
clippy:
    cargo clippy --locked --workspace --all-targets -- -D warnings

# Run the workspace test suite.
test:
    cargo test --locked --workspace

# Install the browser client's locked dependencies.
ui-deps:
    pnpm --dir ui install --frozen-lockfile

# Check and build the browser client.
ui-check: ui-deps
    pnpm --dir ui run check
    pnpm --dir ui run build

# Build the documentation; any rustdoc warning fails.
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps

# Reject Python bytecode and cache directories in the Git index.
check-python-cache:
    python3 scripts/check_python_cache.py

# Compile tracked Python sources in memory without creating bytecode caches.
check-python:
    python3 -c 'import pathlib, subprocess; paths = subprocess.check_output(["git", "ls-files", "-z", "--", "*.py"]).split(b"\0"); [compile(pathlib.Path(path.decode()).read_bytes(), path.decode(), "exec") for path in paths if path]'

# Reject broken links, heading defects, and presentation violations.
check-specs: check-specs-tests
    python3 scripts/check_specs.py --strict

check-specs-tests:
    python3 scripts/test_check_specs.py

# The full gate. Pull requests and main run the same recipes.
ci: check-python-cache check-python check-specs fmt check clippy test doc ui-check

# Run the gate and retain source provenance, output, and exit status.
verify evidence_dir:
    python3 scripts/verify.py {{quote(evidence_dir)}}
