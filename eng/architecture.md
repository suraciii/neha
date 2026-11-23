# Crate boundaries and ownership

## Repository layout

```text
crates/
  neha/       workspace library; capabilities arrive as modules
docs/         user-facing guidance
eng/          engineering practices
scripts/      verification tools
specs/        product contracts
```

The repository root is a virtual manifest. The root `Cargo.toml` declares the members and the shared settings; each crate lives under `crates/`.

## Adding a crate

A crate owns one capability. Add one when a capability needs a dependency set, a failure domain, or a build target that an existing crate cannot own. A line count does not justify a crate.

Each crate inherits `version`, `edition`, `rust-version`, `license`, and `publish` from `[workspace.package]`, and enables the shared lints with `[lints] workspace = true`.

Shared dependency versions are declared once in the root `[workspace.dependencies]` table. A crate then writes `foo.workspace = true`.

## Ownership

One module owns each rule. An adapter may translate between models, but a second copy of a rule is a defect.

This page records boundaries. `specs/` records behavior. When the two disagree, the specification is authoritative, and this page is corrected in the same change.

## See also

- [Documentation language](documentation-language.md).
- [Test architecture](testing.md).
- [Product contracts](../specs/).
