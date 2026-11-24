# Specification Instructions

## Structure

```text
specs/
├── VISION.md              # Product vision (optional)
├── USER-STORIES.md        # User stories
├── AGENTS.md              # This file
└── {topic}/
    ├── VISION.md          # Feature vision (optional)
    ├── product.md         # Required — product behavior
    ├── design.md          # Optional — boundaries, invariants
    └── {sub-topic}/       # Same shape, nested on demand
```

A specification directory is created with the capability it describes. `VISION.md` is created when the product purpose is decided.

## Vision template

Vision headings come from a fixed vocabulary, so every `VISION.md` reads alike:

- **Why** — the problem and who it serves; a user story can open it.
- **What** — the thing itself, as declaratives.
- **Why this way** — the reasons for the chosen shape, and what it protects.
- **Principles** — one rule per bullet, each a test for a change.
- **What this enables** — what the shape makes possible.
- **User stories** — the concrete moments the capability serves.

The product's `VISION.md` uses Why, What, and Principles. A topic's `VISION.md` uses every heading, because a capability earns its place by a problem, a shape, what the shape unlocks, and the moments it serves.

State everything positively: what the product is not follows from what it is, and Why this way states reasons rather than the alternatives it beat. Architecture goes in `design.md`, behavior in `product.md`, and rejected alternatives in the owning Issue or pull request. User stories live in [USER-STORIES.md](USER-STORIES.md); a topic's User stories section links to the entries it serves.

## Rules

- Follow [documentation language rules](../eng/documentation-language.md).
- Write durable target state only.
- Rewrites MUST preserve the target contract. Treat requirement changes as separate product decisions.
- Keep one fact in one authoritative document; link instead of duplicating rules.
- `product.md` states observable product behavior, not implementation or test procedures.
- Define shared domain terms in [CONTEXT.md](../CONTEXT.md). Define capability-specific concepts in the owning spec and link to shared terms.
- Higher layers constrain lower layers. Never the reverse.
- Keep transient material outside the repository.
- Put durable non-contract conclusions in the relevant Issue or pull request.
- Do not use `docs/` as a competing product or design authority.
- **Authority** (optional) — name the owning capability and link to dependencies.
- **Laws** (optional) — state numbered, directly testable invariants that MUST always hold.

## Detail placement

- `product.md` states durable product behavior and invariants. It MUST NOT contain test procedures, fixtures, exact tolerances, command transcripts, or evidence matrices.
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

A capability directory is owned by the capability it names. Its `product.md` owns observable product behavior; its optional `design.md` owns boundaries and invariants. Write each rule in its owning document and link from dependent capabilities.

## Context Layers

- **Product Vision** (`VISION.md`, optional) → why the product exists.
- **Domain Language** ([CONTEXT.md](../CONTEXT.md)) → what each term is.
- **Spec Documents** (`product.md` / `design.md`) → what the product does.
- **Code** → the implementation.
