set dotenv-load := false

# Show the available project commands when no recipe is supplied.
default:
    @just --list

# Check formatting without modifying files.
fmt:
    cargo fmt --all -- --check

# Lint the workspace; any warning fails. Clippy also type-checks every target.
clippy:
    cargo clippy --locked --workspace --all-targets -- -D warnings

# Run the workspace test suite.
test:
    cargo test --locked --workspace

# Run the Space tests with the S3 tests switched on; the environment comes from docs/storage.md.
test-s3:
    cargo test --locked -p neha-space

# Replay synthetic records of 10^5, 10^6, and 10^7 events and report time and
# memory. Not part of the gate: the largest record needs about 5 GB of disk.
bench-replay:
    cargo bench --locked -p neha-space --bench replay

# Run the machine-side tests with the fence switched on; needs bubblewrap (docs/building.md).
test-bwrap:
    cargo test --locked -p neha-computer

# Install the browser client's locked dependencies.
ui-deps:
    pnpm --dir ui install --frozen-lockfile

# Check and build the browser client.
ui-check: ui-deps
    pnpm --dir ui run check
    pnpm --dir ui run build

# Install the desktop shell's locked dependencies.
desktop-deps:
    pnpm --dir apps/desktop install --frozen-lockfile

# Run the desktop shell's unit checks.
desktop-check: desktop-deps
    pnpm --dir apps/desktop test

# Check licenses, advisories, duplicate versions, and registry sources.
deny:
    cargo deny --locked check

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
ci: check-python-cache check-python check-specs fmt clippy test deny doc ui-check desktop-check

# Run the gate and retain source provenance, output, and exit status.
verify evidence_dir:
    python3 scripts/verify.py {{quote(evidence_dir)}}
