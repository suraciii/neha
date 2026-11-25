# Contributing to Neha

Thank you for contributing. This guide covers the contribution process. Build requirements and the full command list live in [Building and verification](docs/building.md).

## Before you open a pull request

- Search open issues and open pull requests for duplicates. Link the closest one, or write "none found".

## Commits

- Follow [Conventional Commits](https://www.conventionalcommits.org/) with a scope, for example `feat(core): add ...`.
- Sign every commit with `git commit -s`.
- Pull requests are squash-merged into `main`; the pull request title becomes the commit subject.

## Gates and evidence

- `just ci` is the required gate: Python hygiene, documentation checks, formatting, type checks, Clippy, tests, and the documentation build.

Automated checks are necessary but not sufficient. Exercise the changed behavior through the real entry point and report the exact scenario and result in the pull request.

| Change | Required evidence |
| --- | --- |
| Behavior | The scenario, command or interface used, and the observed result |
| Numerical or performance claim | Measured before and after values, with the measurement conditions |
| Documentation or contract | `just check-specs` output and the affected documents |
| Build or CI | The gate result and any environment limit |

## Standards

- [Coding standards](CODING_STANDARDS.md) — reviewer judgment rules.
- [Test architecture](eng/testing.md) — test requirements and the gate.
- [Architecture](eng/architecture.md) — processes, planes, and layout.
- [specs/](specs/) — product contracts; [docs/](docs/README.md) — user guides.

## Language

Issues, pull requests, commits, and documentation are written in English. See [documentation language](eng/documentation-language.md).

## License

Neha is licensed under the [GNU General Public License v3.0](LICENSE), identified as `GPL-3.0-only`. Contributions are accepted under the same license. Do not add third-party code under an incompatible license.
