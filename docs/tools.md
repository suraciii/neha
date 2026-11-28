# Let a Bot work on its Computer

This guide shows a Bot using the built-in tools to work in its own directory inside its Computer's Workspace, and on a Hosted Computer in the rest of that Workspace: a shell, file reads, file writes, and file listing. It follows [Quick start](quickstart.md), so the Space, the Admin, the Provider, the Bot, and the Chat already exist. It describes a Bot on a Hosted Computer; [Computers](computer.md) covers a machine of its own.

## What the Bot has

A Bot has four tools, all starting in its own directory:

| Tool | What it does |
| --- | --- |
| `shell` | Runs a POSIX shell command in the Bot's directory and returns the exit code, stdout, and stderr. On a Hosted Computer the shell can reach the rest of the Computer's Workspace. |
| `read_file` | Reads a file the Computer's rules allow. On a Hosted Computer that is any file in the Computer's Workspace; on a Local Computer it is the Bot's directory, a granted folder, or the run's directory. |
| `write_file` | Writes a file the Computer's rules allow, under the same range. |
| `list_files` | Lists a directory the Computer's rules allow, under the same range. |

Files move through the Bot's shell with `neha file get` and `neha chat send --file`; [Send and receive Files](files.md) covers them. On a Local Computer, every tool action goes through the approval gate; [Approvals](approvals.md) covers when it asks.

The loop offers these tools to the Model. When the Model asks for a tool, the Space records the action, runs it on the Bot's Computer, records the result, and gives the result back to the Model. The Bot can take several steps this way before it answers in the Chat.

A Hosted Computer keeps its Workspace root in the Space's home, and the Bot starts in its own directory inside it:

```text
$NEHA_HOME/workspaces/<computer-id>/<bot-id>
```

On a Hosted Computer, the sandbox mounts the Computer's Workspace root at `/workspace` and starts the shell in the Bot's directory inside it; [Computers](computer.md#the-hosted-computer) covers the sandbox and the Workspace root it lives in.

## Ask for work

Ask in the Chat. The Bot chooses the tools:

```bash
target/debug/neha chat send "Create notes.txt with the text 'remember the milk', then read it back." | tee run.txt
RUN_ID=$(awk '/^  run / {print $2; exit}' run.txt)
```

The reply names what the Bot found. The run shows every step and action:

```bash
target/debug/neha run show "$RUN_ID"
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

- On a Hosted Computer a path must stay inside the Computer's Workspace; on a Local Computer it must stay inside the Bot's directory, a granted folder, or the run's directory. Absolute paths outside that range, `..` climbs, and any symlink in the path are refused, including a dangling symlink a write could follow.
- A shell command has 30 seconds. When it runs past the limit, the Space kills the whole process group.
- The shell starts with an empty environment plus `PATH`, `HOME`, `TMPDIR`, and the Bot's `NEHA_BOT_TOKEN`, `NEHA_SPACE_URL`, and `NEHA_CHAT`. On a Hosted Computer `HOME` is the Bot's own directory, which appears as `/workspace/<bot-id>` inside the sandbox whose root is the Computer's Workspace; on a Local Computer it is the Bot's directory on the machine. `TMPDIR` is the sandbox's own `/tmp`; Provider keys and other Space secrets are not in that environment.
- On a process-local Hosted Computer, the shell runs in a `bubblewrap` sandbox: the machine's system directories are read-only, the Computer's Workspace root is the only place where files persist, and the process list, `/dev`, `/proc`, and `/tmp` are the sandbox's own. On a worker-backed Hosted Computer, the container is the fence instead: the shell runs in the container with a read-only root filesystem, its own `/tmp` tmpfs, and no nested sandbox. Either way Bots on the same Hosted Computer can reach each other's directories in the Workspace, and the shell cannot see the Space's record, secrets, File store, or another Computer's Workspace.
- On a Local Computer in granted mode, the shell runs in a sandbox that allows persistent writes only in the Bot's own directory, the folders its owner granted, and the run's `--cwd` directory; with full access the shell runs directly as the machine's user, with no sandbox. The file tools check their own write paths. Reading follows the machine user's rights, except the Computer's configuration directory in granted mode, so run only prompts you trust.
- A tool result keeps its first 8 KiB and last 24 KiB and names the omitted byte count, including output beyond the capture limit. A write is limited to 64 KiB. A file read reads at most 8 MiB, then shows its first 8 KiB and last 24 KiB with the omitted count.
- A run stops after 8 steps. If the Model keeps asking for tools, the Bot answers with a failure Message and the Space stays available.

## What the milestone does not do

- A process-local Hosted Computer's sandbox is not an independent machine image: it has no screen or browser and shares the Space machine's network.
- A worker container has a minimal operating system. It has the shell and the tools the worker image installs, not the Space host's system directories; a package the Bot needs must be in the worker image or installed in its Workspace.
- An unattended approval does not expire yet, and a Hosted Computer does not ask before an action. [Approvals](approvals.md) covers the gate that works on a Local Computer.
- A Local Computer runs the same tools in the Bot's own directory inside its Workspace on that machine. The file tools check write paths. In granted mode the shell runs in a sandbox that allows persistent writes only in the Bot's own directory, the folders its owner granted, and the run's `--cwd` directory; with full access the shell runs directly as the machine's user, with no sandbox. [Computers](computer.md#the-local-computers-workspace-and-the-write-fence) covers the range and [offline behavior](computer.md#while-the-machine-is-offline).
