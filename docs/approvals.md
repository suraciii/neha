# Approvals

This guide shows when a Bot asks before an action, how to answer, and how to make lasting Ask and Allow rules. It follows [Computers](computer.md), so a Local Computer is paired and a Bot works on it.

## When a Bot asks

A Bot runs a Computer action without asking by default. It asks before the first action it runs on a Local Computer, once for each Bot, acting User, and Local Computer, and before an action that a lasting Ask rule names. A Bot moved to another Local Computer asks again on its first action there.

A Bot also asks before the first run of a skill's script, on a Hosted or a Local Computer, once for each Bot, acting User, skill, and script. [Skills](skills.md#scripts) covers that action.

Files move through the Bot's shell (`neha file get` and `neha chat send --file`), so they wait on the shell action's approval; there is no separate File action.

In granted mode a `write_file` that would write outside the Bot's own directory inside its Computer's Workspace, its granted folders, and the run's `--cwd` directory asks as well, when the Space can locate the target path. Allow runs it once, Always allow grants the folder for later runs, and Deny ends the run. A shell that writes outside that range is refused by the fence rather than turned into a request, and a Bot with full access never gets a folder card. [Computers](computer.md#file-access-and-a-runs-directory) covers the fence and the commands that widen it.

Only the User the Bot acts for can answer. An Allow runs the action once; an Always allow also covers later actions of that kind.

## Answer in the Chat

Run `neha chat send` as usual. When the run waits, a terminal shows the approval card: the action's tool, a summary, why the Bot asks, whether the action can be undone, and, for a rule that offers it, what Always allow covers. A `shell` or `write_file` action says that it cannot be undone; a `read_file`, `list_files`, or other action says that the undoability is unknown. An ambiguous form gives the reason `a form no rule can judge`, not a rule you set. The summary escapes control characters, and one longer than 4096 bytes says how many bytes it left out. Answer with:

```text
[a]llow / [A]lways allow / [d]eny / [q]uit
```

The first use of a Local Computer offers no Always allow. `q` leaves the run waiting, and the command prints the run and approval ids.

Without a terminal, the command prints the ids and stops instead of waiting:

```text
run <run-id> is waiting for approval <approval-id>; answer with `neha approval allow <approval-id>`
```

## Answer from another command

List the approvals that wait for you:

```bash
target/debug/neha approval list
```

Each line gives one approval's id, run, Bot, tool, reason, and summary, and `neha run show <run>` shows the run behind one. Several approvals can wait at once, so the Bot and the run name the one you mean; put that approval's id in the shell:

```bash
APPROVAL_ID="<the approval you mean>"
target/debug/neha approval show "$APPROVAL_ID"
```

`neha approval show <approval-id>` prints where the action sits, the directory its run works in when the run named one, and its full arguments. The directory its run works in is read exactly as the run recorded it, so a name that ends in a space keeps it and a relative path is matched against that same directory. `neha approval allow <approval-id> --always` also makes the action's kind a lasting Allow rule, and it is refused for a first use. On an approval for a folder the owner has not granted, `--always` grants that folder instead, and `neha bot access <bot-id>` shows it among the Bot's grants. On a Computer that cannot serve granted folders, `--always` is refused instead and the approval still waits.

Answer it one way or the other, never both:

```bash
target/debug/neha approval allow "$APPROVAL_ID"
```

```bash
target/debug/neha approval deny "$APPROVAL_ID"
```

A Deny ends the run, and the Bot says that it stopped.

## What an Always allow covers

An Always allow covers the same tool and one of these:

- every shell command whose text starts with the approved command's first word: `printf` covers `printf 'hello'` and `printfx`;
- every path at or below the approved action's directory;
- the same skill and script for a `run_skill_script` action;
- every action of a tool with no command or path.

An Always allow derives its rule from the action itself: a command gives its first word, a path gives its parent directory, a skill's script gives its skill and script, an MCP server's tool gives that server and the tool, and only a Computer action with neither gives a tool-only rule.

A rule-triggered approval offers no Always allow, and `--always` is refused, when the action cannot make a lasting rule: a path with a `..` or all-whitespace component, a shell command with no first word, or a file action with no usable path. A folder approval offers no Always allow when the folder no grant can store — a `..` climb, a relative form, or a NUL — and `--always` is refused for it; Allow still runs that one action. An ambiguous form that can still make a lasting rule does offer Always allow, and that answer removes no rule.

For a file at the Workspace root, the covered directory is the Workspace root: relative paths are covered and absolute ones are not. When a rule asks, the card names the scope: `shell commands starting with "printf"`, `files under "logs"`, `files in the Workspace root`, or `all <tool> actions`. The first use of a Local Computer never offers Always allow, and no lasting rule covers it.

## Lasting rules

List, add, and remove lasting rules, with the Bot's id in `BOT_ID`:

```bash
target/debug/neha rule list
BOT_ID=$(target/debug/neha bot list | awk '$2 == "Runner" {print $1; exit}')
target/debug/neha rule add --bot "$BOT_ID" --effect ask --tool shell --command-prefix rm
target/debug/neha rule add --bot "$BOT_ID" --effect allow --tool read_file --path-prefix /home/ada/docs
target/debug/neha rule add --bot "$BOT_ID" --effect ask --tool write_file --path-prefix /home/ada/reports
target/debug/neha rule add --bot "$BOT_ID" --effect ask --tool run_skill_script --skill weekly-report --script collect.sh
```

`neha rule add` prints the rule it recorded as `Added rule <rule-id> …`, so the command that adds a rule also names it. Remove a rule by the id its own `rule add` printed, not by the first line of `rule list`, whose top row can belong to another Bot:

```bash
RULE_ID=$(target/debug/neha rule add --bot "$BOT_ID" --effect allow --tool shell --command-prefix ls | awk '{print $3; exit}')
target/debug/neha rule remove "$RULE_ID"
```

An Ask rule makes the Bot ask before that kind of action, and an Allow rule lets it proceed without asking. A `run_skill_script` rule names a skill and a script instead of a prefix, so `--skill` and `--script` go with `--tool run_skill_script`. A rule for an MCP server's tool names the server and the tool's original name, so `--server` goes with `--tool <tool>` and no prefix; without `--server`, an MCP tool's name matches every enabled server that offers that tool, and a name no enabled server that the acting User can reach lists is refused as unknown. `neha rule list` shows a rule's server by name, and a script rule's skill and script, so two servers that offer the same tool are told apart and a script rule never reads as covering every script.

A rule about a Computer action belongs to a Bot on a Local Computer; a Hosted Computer never asks, so it refuses one. A rule about a Space-served call — a skill's script or an MCP server's tool — is recorded for a Bot on either Computer, because the Space asks for those calls whatever the Computer.

When both match, the more specific rule wins: a rule with a command or path prefix beats one with only the tool, a longer prefix beats a shorter one, and a skill and script beat both. If they are equally specific, the Ask rule wins.

Some forms a prefix cannot judge. A shell command with `;`, `&`, `|`, `$`, a backtick, a parenthesis, `<`, `>`, or a line break asks whenever the Bot and User have any shell rule: `echo hi; rm x` is never covered by an `rm` rule. A file path with `..`, or an absolute/relative form unlike the rule's prefix, asks whenever the Bot and User have any rule for that tool: a `logs` rule does not cover `../logs/x` or `/var/logs/x`. With no rule for the tool, the action runs. An approval raised by this protection reports `ambiguous` true and names no rule: `neha approval show` shows none, and its Always allow removes no rule.

`neha rule add` trims and normalizes a path prefix once, and the Space stores that form; matching does not trim the stored prefix again. A raw action path is trimmed once, then compared after collapsing repeated `/` and removing `.` components, while keeping the absolute or relative form. So `./logs/x`, `logs//x`, and `logs/./x` match a `logs` rule; `.` matches `./a.txt` but no absolute path. A `..` component is not removed and still follows the fail-closed rule above.

A path made only of whitespace is ambiguous when the tool has a rule. A directory name that ends with a space is a scope of its own: `logs` and `logs ` differ.

### A wide Ask rule and an Always allow

Create an Ask rule for every shell command:

```bash
target/debug/neha rule add --bot "$BOT_ID" --effect ask --tool shell
```

Before the Bot runs `printf 'hello'`, the approval asks. Choose Always allow: Neha adds an Allow rule for `shell` with the command's first word, `printf`. The Ask rule has no prefix, so the new Allow rule does not cover it: other shell commands still ask, and `printf` commands no longer ask because the longer prefix wins.

### A narrow Ask rule and an Always allow

Instead, make the Ask rule name only `printf` commands:

```bash
target/debug/neha rule add --bot "$BOT_ID" --effect ask --tool shell --command-prefix printf
```

Choose Always allow on that approval: the new Allow rule has the same `printf` prefix. It covers the Ask rule, so Neha removes the Ask rule, and `printf` commands no longer ask.

The same holds for a skill's script: an Ask rule on the pair makes each run ask, and Always allow on that card writes an Allow rule for the pair and removes the Ask rule it covers. A server's tool works the same way: an Ask rule on its server and tool makes each call ask, and Always allow writes the matching Allow rule.

## While a run waits

A waiting approval does not block the Bot: a later request for the same Bot runs while the first waits. The waiting run resumes after its last recorded action when allowed, and no action runs twice. A Space restart continues a working run from its record: completed steps replay, and an action that started before the stop is checked rather than run again.

## What this milestone does not do

- A Question from the Bot, and a Model asking for approval on its own, are not available.
- An approval of a run no User is with does not expire yet.
- The other actions that always ask, such as paying or filling a saved credential, are not available.
- A Group approval, cancelling an approval, and undoing a decision are not available.
- A Hosted Computer does not ask before the actions it runs yet, except before the first run of a skill's script and the first call of an MCP server's tool.
- The Bot's page does not show approvals yet.
- An independent reviewer of approvals is not available.
