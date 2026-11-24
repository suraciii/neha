# Coding review standards

This file contains reviewer-only judgment rules. Read it when a change touches a data structure, serialized document, default, enum wire value, or module boundary.

## Data structures and wire models

A review is complete only when it names the authoritative owner, the affected wire behavior, the consumers that depend on it, and the evidence that compatibility is preserved or intentionally changed.

- Identify the domain owner before reviewing an extraction or redesign. Keep one typed model authoritative. An adapter may translate to a wire model, but it must not create a second competing vocabulary.
- Follow the owning specification and design document before evaluating fields, defaults, enum names, optionality, or nested boundaries. Product behavior belongs in `product.md`; ownership and invariants belong in `design.md`.
- Treat Serde attributes as wire behavior. Review `deny_unknown_fields`, `default`, `skip`, renames, custom deserializers, omitted fields, JSON `null`, and version migrations together with their consumers.
- Trace every structural change across the surfaces that consume the type: runtime parameters, CLI sources, presets, state documents, reports, and generated artifacts. A compile pass does not prove wire compatibility.
- Prefer a narrow domain module over a line-count split. A child module must own a coherent data boundary and must not duplicate validation, defaults, or serialization rules already owned elsewhere.
- Preserve unknown-field rejection, omitted-field behavior, nullability, numeric precision, enum spelling, and schema-version policy unless the governing specification changes in the same patch.
- Use existing round-trip, rejection, migration, and real-consumer tests. Add a permanent test only for a consumer-visible boundary or an uncertain compatibility edge; do not test field copies or serializer wiring in isolation.

## Module boundaries

- A new module must own a rule that no other module owns. Moving code without moving ownership is not a refactor.
- A public type must express a domain concept. A wrapper that only forwards calls is a shallow layer; remove it or deepen it.
- Keep error messages stable when callers or tests match them. A message change is a behavior change.

## Verification evidence

Run the relevant commands from [Justfile](Justfile). `just ci` is the full gate; report its result rather than describing the change.
