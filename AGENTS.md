# Agent Instructions

## Project

Neha is a Rust workspace. Product scope lives in [specs/](specs/); the domain language lives in [CONTEXT.md](CONTEXT.md).

- [specs/](specs/) — product contracts: observable behavior, boundaries, invariants.
- [specs/client/design-system.md](specs/client/design-system.md) — UI/UX design language and system; read before UI work.
- [docs/](docs/README.md) — user-facing guidance.
- [eng/](eng/) — engineering practices: [architecture](eng/architecture.md), [testing](eng/testing.md), [documentation language](eng/documentation-language.md).
- [CODING_STANDARDS.md](CODING_STANDARDS.md) — reviewer judgment rules.

## Engineering

- Follow KISS and YAGNI. Reuse existing boundaries and dependencies.
- Keep each rule in one authoritative implementation.
- Preserve observable behavior and public contracts.
- Require measured evidence for numerical and performance claims.
- Keep plans, research, probes, logs, build artifacts, and session state outside the repository.
- Put durable non-contract conclusions in the relevant Issue or pull request. Never commit transient artifacts, even if ignored.
- Write tests that catch meaningful failures. See [eng/testing.md](eng/testing.md).
- For UI work, read [product.md](specs/client/product.md) for behavior and [design.md](specs/client/design.md) for technical boundaries; exact visual values live in [styles.css](ui/src/styles.css).
- A test that takes minutes has no value: an assertion nobody runs protects nothing. Measure a test's cost before adding it, and delete a slow test rather than quarantining it.

## Verification

- Use the commands in [Justfile](Justfile); dependencies are listed in [docs/building.md](docs/building.md).
- Before handing off code, run `just ci` and report its result.
- Exercise changed behavior through the real entry point.
- Report failures, environment limits, and the scope of the evidence.
