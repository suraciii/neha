# Let a Bot work on its Computer

This guide shows a Bot using the built-in tools to work in its Computer's workspace: a shell, file reads, file writes, and file listing. It follows [Quick start](quickstart.md), so the Space, the Admin, the Provider, the Bot, and the Chat already exist. It describes a Bot on a Hosted Computer; [Add a Local Computer](computer.md) covers a machine of its own.

## What the Bot has

A Bot has four tools, all working in the workspace:

| Tool | What it does |
| --- | --- |
| `shell` | Runs a POSIX shell command in the workspace and returns the exit code, stdout, and stderr. |
| `read_file` | Reads a file in the workspace. |
| `write_file` | Writes a file in the workspace. |
| `list_files` | Lists a directory in the workspace. |

Files move through the Bot's shell with `neha file get` and `neha chat send --file`; [Send and receive Files](files.md) covers them. On a Local Computer, every tool action goes through the approval gate; [Approvals](approvals.md) covers when it asks.

The loop offers these tools to the Model. When the Model asks for a tool, the Space records the action, runs it on the Bot's Computer, records the result, and gives the result back to the Model. The Bot can take several steps this way before it answers in the Chat.

The workspace lives under `NEHA_HOME`:

```text
$NEHA_HOME/computers/<computer-id>/workspace
```

On a Hosted Computer, the shell sees this folder at `/workspace`; [Add a Local Computer](computer.md#the-hosted-computer) covers the sandbox.

## Ask for work

Ask in the Chat. The Bot chooses the tools:

```bash
target/debug/neha chat send "Create notes.txt with the text 'remember the milk', then read it back."
```

The reply names what the Bot found. The run shows every step and action:

```bash
target/debug/neha run show <run-id>
```

```text
Run 0193...: succeeded
  reply 0193...
  usage {"cache_read":0,"cache_write":0,"output":35,"reasoning":0,"uncached_input":120}
  step 1: 1 tool call(s)
  step 2: 1 tool call(s)
  step 3: 0 tool call(s)
  action 1 (step 1): write_file (succeeded)
    wrote 16 bytes to notes.txt
  action 2 (step 2): read_file (succeeded)
    remember the milk
```

A shell command works the same way:

```bash
target/debug/neha chat send "Run printf 42 and tell me what it printed."
```

Add `--follow` to print steps and actions while the run works:

```bash
target/debug/neha chat send --follow "List the workspace, then write a summary."
```

The final reply still prints on standard output; progress prints on standard error.

## Limits

The runtime enforces these limits, and an action that breaks one returns an error to the Model instead of failing the whole run:

- A path must stay inside the workspace. Absolute paths outside it, `..` climbs, and any symlink in the path is refused, including a dangling symlink a write could follow.
- A shell command has 30 seconds. When it runs past the limit, the Space kills the whole process group.
- The shell starts with an empty environment plus `PATH`, `HOME`, `TMPDIR`, and the Bot's `NEHA_BOT_TOKEN`, `NEHA_SPACE_URL`, and `NEHA_CHAT`. `HOME` is the Workspace, and `TMPDIR` is the sandbox's own `/tmp`; Provider keys and other Space secrets are not in that environment.
- On a Hosted Computer, the shell runs in a `bubblewrap` sandbox: the machine's system directories are read-only, the Workspace is the only place where files persist, and the process list, `/dev`, `/proc`, and `/tmp` are the sandbox's own. The sandbox shares the machine's network and cannot see the Space's record, secrets, File store, or another Computer's Workspace. [Computers](computer.md#the-hosted-computer) covers the sandbox and the layouts it refuses.
- On a Local Computer in granted mode, the shell runs in a sandbox that allows persistent writes only in the Workspace, the folders its owner granted, and the run's `--cwd` directory; with full access the shell runs directly as the machine's user, with no sandbox. The file tools check their own write paths. Reading follows the machine user's rights, except the Computer's configuration directory in granted mode, so run only prompts you trust.
- A tool result keeps its first 8 KiB and last 24 KiB and names the omitted byte count, including output beyond the capture limit. A write is limited to 64 KiB. A file read reads at most 8 MiB, then shows its first 8 KiB and last 24 KiB with the omitted count.
- A run stops after 8 steps. If the Model keeps asking for tools, the Bot answers with a failure Message and the Space stays available.

## What the milestone does not do

- A Hosted Computer's sandbox is not an independent machine image: it has no screen or browser and shares the Space machine's network.
- An unattended approval does not expire yet, and a Hosted Computer does not ask before an action. [Approvals](approvals.md) covers the gate that works on a Local Computer.
- A Local Computer runs the same tools in the Bot's Workspace on that machine. The file tools check write paths. In granted mode the shell runs in a sandbox that allows persistent writes only in the Bot's Workspace, the folders its owner granted, and the run's `--cwd` directory; with full access the shell runs directly as the machine's user, with no sandbox. [Add a Local Computer](computer.md#the-bots-workspace-and-the-write-fence) covers the range and [offline behavior](computer.md#while-the-machine-is-offline).
