# Approvals

This guide shows when a Bot asks before an action, how to answer, and how to make lasting Ask and Allow rules. It follows [Add a Local Computer](computer.md), so a Local Computer is paired and a Bot works on it.

## When a Bot asks

A Bot runs a Computer action without asking by default. It asks before the first action it runs on a Local Computer, once for each Bot and acting User, and before an action that a lasting Ask rule names.

Only the User the Bot acts for can answer. An Allow runs the action once; an Always allow also covers later actions of that kind.

## Answer in the Chat

Run `neha chat send` as usual. When the run waits, a terminal shows the approval card: the action's tool, a summary, why the Bot asks, and whether the action can be undone. Answer with:

```text
[a]llow / [A]lways allow / [d]eny / [q]uit
```

The first use of a Local Computer offers no Always allow. `q` leaves the run waiting, and the command prints the run and approval ids.

Without a terminal, the command prints the ids and stops instead of waiting:

```text
run <run-id> is waiting for approval <approval-id>; answer with `neha approval allow <approval-id>`
```

## Answer from another command

List the approvals that wait for you, then answer one:

```bash
target/debug/neha approval list
target/debug/neha approval allow <approval-id>
target/debug/neha approval deny <approval-id>
```

`neha approval allow <approval-id> --always` also makes the action's kind a lasting Allow rule, and it is refused for a first use. A Deny ends the run, and the Bot says that it stopped.

## What an Always allow covers

An Always allow covers the same tool and one of these:

- a shell command that starts with the same first word;
- a file action in the same directory.

The first use of a Local Computer never offers Always allow, and no lasting rule covers it.

## Lasting rules

List, add, and remove lasting rules:

```bash
target/debug/neha rule list
target/debug/neha rule add --bot <bot-id> --effect ask --tool shell --command-prefix rm
target/debug/neha rule add --bot <bot-id> --effect allow --tool read_file --path-prefix /home/ada/docs
target/debug/neha rule remove <rule-id>
```

An Ask rule makes the Bot ask before that kind of action, and an Allow rule lets it proceed without asking.

When both match, the more specific rule wins: a rule with a command or path prefix beats one with only the tool, and a longer prefix beats a shorter one. If they are equally specific, the Ask rule wins.

### A wide Ask rule and an Always allow

Create an Ask rule for every shell command:

```bash
target/debug/neha rule add --bot <bot-id> --effect ask --tool shell
```

Before the Bot runs `printf 'hello'`, the approval asks. Choose Always allow: Neha adds an Allow rule for `shell` with the command's first word, `printf`. That Allow rule is narrower than the Ask rule, so the Ask rule stays; other shell commands still ask, and `printf` commands no longer ask.

### A narrow Ask rule and an Always allow

Instead, make the Ask rule name only `printf` commands:

```bash
target/debug/neha rule add --bot <bot-id> --effect ask --tool shell --command-prefix printf
```

Choose Always allow on that approval: the new Allow rule has the same `printf` prefix. It covers the Ask rule, so Neha removes the Ask rule, and `printf` commands no longer ask.

## While a run waits

A waiting approval does not block the Bot: a later request for the same Bot runs while the first waits. The waiting run resumes after its last recorded action when allowed, and no action runs twice.

## What this milestone does not do

- A Question from the Bot, and a Model asking for approval on its own, are not available.
- An approval of a run no User is with does not expire yet.
- The other actions that always ask, such as paying or filling a saved credential, are not available.
- A Group approval, cancelling an approval, and undoing a decision are not available.
- A Hosted Computer does not ask for approvals yet.
- The Bot's page does not show approvals yet.
- An independent reviewer of approvals is not available.
