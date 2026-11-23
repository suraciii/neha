# Specification Instructions

## Structure

```text
specs/
├── VISION.md              # Product vision (optional)
├── AGENTS.md              # This file
└── {topic}/
    ├── VISION.md          # Feature vision (optional)
    ├── spec.md            # Required — product behavior
    ├── design.md          # Optional — boundaries, invariants
    └── {sub-topic}/       # Same shape, nested on demand
```

A specification directory is created with the capability it describes. `VISION.md` is created when the product purpose is decided.

## Rules

- Follow [documentation language rules](../eng/documentation-language.md).
- Write durable target state only.
- Rewrites MUST preserve the target contract. Treat requirement changes as separate product decisions.
- Keep one fact in one authoritative document; link instead of duplicating rules.
- `spec.md` states observable product behavior, not implementation or test procedures.
- Define shared domain terms in [CONTEXT.md](../CONTEXT.md). Define capability-specific concepts in the owning spec and link to shared terms.
- Higher layers constrain lower layers. Never the reverse.
- Keep transient material outside the repository.
- Put durable non-contract conclusions in the relevant Issue or pull request.
- Do not use `docs/` as a competing product or design authority.
- **Authority** (optional) — name the owning capability and link to dependencies.
- **Laws** (optional) — state numbered, directly testable invariants that MUST always hold.

## Detail placement

- `spec.md` states durable product behavior and invariants. It MUST NOT contain test procedures, fixtures, exact tolerances, command transcripts, or evidence matrices.
- Tests own concrete cases, fixtures, tolerances, byte-identity checks, failure injection, and step sequences.
- Help, schemas, and code own exhaustive field, option, and parameter inventories.
- Issues and pull requests own migration steps, implementation gaps, counts, dates, and rejected alternatives.
- `## Acceptance` is optional and states at most three observable outcomes.

## Presentation

Choose the smallest form that makes the target contract clear:

| Content | Form |
| --- | --- |
| Branching behavior or decision order | Non-executable, contract-level pseudocode |
| States and transitions | Mermaid state diagram |
| Interactions or data flow | Mermaid sequence or flow diagram |
| Ownership or hierarchy | Shallow text tree or Mermaid |
| Fields, independent conditions, or comparisons | Markdown table |
| Simple requirement or exception | Short sentence |

- A prose paragraph carries one rule and MUST NOT exceed three sentences or 60 words.
- A bullet carries one rule and MUST NOT exceed two sentences.
- A sentence MUST NOT exceed 40 words.
- Three or more parallel items use a table or a list, never a comma-separated sentence.
- Diagrams, tables, and pseudocode state the contract; do not repeat them in prose.
- Make conditions, outcomes, scope, and failure effects explicit.
- Keep visuals in the owning Markdown document, with scope statements and constraints beside the content they qualify.

## Ownership

A capability directory is owned by the capability it names. Its `spec.md` owns observable product behavior; its optional `design.md` owns boundaries and invariants. Write each rule in its owning document and link from dependent capabilities.

## Context Layers

- **Product Vision** (`VISION.md`, optional) → why the product exists.
- **Domain Language** ([CONTEXT.md](../CONTEXT.md)) → what each term is.
- **Spec Documents** (`spec.md` / `design.md`) → what the product does.
- **Code** → the implementation.
