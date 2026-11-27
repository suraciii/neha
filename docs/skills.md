# Skills

This guide shows how to give a Bot a skill, how the Bot loads it, and what happens when a skill carries a script.

A skill is one `SKILL.md` file with a name, a description, and a body of instructions. Neha reads the [Agent Skills](https://agentskills.io) format: a YAML frontmatter between `---` lines, then the body. Only `name` and `description` are required; every other frontmatter field is ignored.

```markdown
---
name: weekly-report
description: Write the weekly status report from the docs the User names.
---

1. Collect the docs the User names.
2. Summarize them under one page.
```

A name is 1 to 64 lowercase letters, digits, or hyphens, and it does not begin or end with a hyphen. A description has 1 to 1024 characters. The body is at most 64 KiB.

## Add a skill

Put the skill in a directory. Scripts go in a `scripts/` folder next to `SKILL.md`:

```text
weekly-report/
  SKILL.md
  scripts/
    collect.sh
```

Then add the directory:

```bash
target/debug/neha skill add ./weekly-report
target/debug/neha skill add ./weekly-report --bot <bot-id>
```

Without `--bot` the skill joins the Space's library, which only the Admin may add to. With `--bot` it joins that Bot's library, and only the Bot's owner may add it there. A library holds one skill under each name; remove the old one before adding another under the same name.

```bash
target/debug/neha skill list
target/debug/neha skill remove <skill-id>
```

## Give a skill to a Bot

A skill in the Space's library reaches a Bot only when the owner enables it. A skill in a Bot's own library reaches it the same way.

```bash
target/debug/neha bot skill enable <bot-id> <skill-id>
target/debug/neha bot skill disable <bot-id> <skill-id>
```

Removing a skill takes it away from every Bot that enabled it.

## What the Bot sees

An enabled skill's name and description enter the Bot's context before each run. The body enters only when the Bot loads it with the `skill` tool, so a large body costs tokens only when the task matches the description.

Loading a skill touches no Computer and asks no approval.

## Scripts

A Bot runs a script a skill carries with the `run_skill_script` action on its Computer. The Space passes the script's exact bytes to the Computer, which runs them under the same fence as a shell command and returns their output. A fenced Computer mounts the bytes read-only inside its sandbox: no file in the Workspace holds the script, so the file a run executes is the one the Space recorded.

The first run of one skill's script asks for approval, once for each Bot, acting User, skill, and script. Always allow makes a lasting Allow rule for that skill and script alone, so another script of the same skill asks on its first run. The reason on the card is `first_script_run`.

An Ask rule for that skill and script makes every run ask. Always allow on that card answers it and removes the Ask rule it covers. [Approvals](approvals.md#lasting-rules) covers the rule commands.

A script runs under the same fence, time limit, and output limit as a shell command, and `write_file` rules do not cover it. A script that the Bot does not have never runs: the action fails without asking. A script never runs on a Windows Computer yet: the action fails and the card offers no way to run it.

The body says which script to run and what to pass. The Bot names the script by its file name inside `scripts/`.

## Limits

- `neha skill add` reads `SKILL.md` and every regular file directly in `scripts/`: at most 16 scripts, each at most 64 KiB, and a document of at most 256 KiB.
- `neha skill add` refuses a symlink and a file that is not a regular file, at every level, so a skill's files come from the directory the User named.
- A script name is one path component of letters, digits, dots, hyphens, or underscores, so a name cannot leave the staging directory.
- The Space parses `SKILL.md` as YAML before it records anything, and it reports the frontmatter rule a rejected document broke.
- The Admin can set the Space to treat skills as instructions only. Add `[skills] instructions_only = true` to `config.toml` in the Space home and restart the Space; then no skill's script is offered to a Bot or reaches a Computer, and a call to one fails.

## What this milestone does not do

- A skill comes from one directory the User names. Installing a package that carries skills is not available yet.
- The browser client does not show or enable skills yet.
- A skill cannot require a tool, and no frontmatter field other than `name` and `description` changes what Neha does.
