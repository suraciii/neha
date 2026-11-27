# Send and receive Files

This guide shows a file going to a Bot and a result coming back: a User sends a file in a Chat, the Bot takes it into its Workspace, and the Bot delivers a result as a File. It follows [Quick start](quickstart.md), so the Space, the Admin, the Provider, the Bot, and the Chat already exist.

## What a File is

A File is a file sent in a Chat: an image, a document, a spreadsheet, or anything else. The Space stores it, and it belongs to the Chat it was sent in.

- A Message carries one File. Send two Messages for two files.
- A File stays in the Chat when the Space restarts, and every client that reads the Message downloads the same File.
- The size limit is in [Limits](#limits), and what a Bot does with a File is in [Let the Bot take and deliver](#let-the-bot-take-and-deliver).

## Send a file from the CLI

`--file` sends a file with a Message and waits for the Bot's reply, as `chat send` does:

```bash
printf 'remember the milk\n' > notes.txt
target/debug/neha chat send --file notes.txt "Summarize this file."
```

After the reply and the run, the CLI prints a `file` line with the id and name of the File you sent.

The text is optional. A Message can carry a File and no text:

```bash
printf 'buy milk\n' > todo.txt
target/debug/neha chat send --file todo.txt
```

With several Chats, name the Chat or the Bot as for any `chat send`:

```bash
BOT_ID=$(target/debug/neha bot list | awk '$2 == "Scout" {print $1; exit}')
target/debug/neha chat send --file notes.txt --bot "$BOT_ID" "Summarize this file."
```

The Space records a File as soon as it has the file's bytes; the Message carries it after that. When a Message then fails to send, the File is in the Chat's list already, and `neha chat files` shows it.

Sending the same file to the same Chat again adds no File. The Space answers with the File it already recorded, and the new Message carries that File.

## List and download Files from the CLI

List the Files of a Chat, in the order the Space recorded them. With one Chat, no identifier is needed:

```bash
target/debug/neha chat files
```

```text
0193...  notes.txt  18 bytes  text/plain
0193...  todo.txt  9 bytes  text/plain
```

Each line shows one File's id, name, size, and media type. `neha chat read <chat-id>` also names a Message's File on a `file` line under the Message.

Download a File:

```bash
FILE_ID=$(target/debug/neha chat files | awk '$2 == "notes.txt" {print $1; exit}')
target/debug/neha file get "$FILE_ID"
```

The command prints the path it wrote. Where the File goes follows `--out`:

- Without `--out`, the File is written into the current directory under the name the Space gave it. A file of that name already there is not replaced; the command stops and asks for `--out`.
- With a path that is not an existing directory, the File is written to that path, replacing what is there.
- With an existing directory, the File is written inside it under the name the Space gave it.

```bash
target/debug/neha file get "$FILE_ID" --out summary.md
```

## Send and download in the browser

Open the Chat. The paperclip button at the left of the composer opens a file picker. The chosen file then shows as a chip above the input with its name and size; its × removes it.

Send the Message as usual, with or without text. A Message with a File shows a file card with the file's name and size; choosing the card downloads the File, the same File on every client.

When the file is over the limit or the upload fails, the Chat says so and keeps the Message in the composer.

## Let the Bot take and deliver

The Bot works with Files through the `neha` command in its shell, not through File tools. A run's shell already has `NEHA_BOT_TOKEN`, `NEHA_SPACE_URL`, and `NEHA_CHAT`, so these commands work without a sign-in:

- `neha file get <id>` writes a File of the Chat into the Workspace under the File's name; it writes there even when the shell runs in another directory. With `--out`, the shell runs the CLI with its usual write rights.
- `neha chat send --file <path>` sends a Workspace file to the Chat as a Bot Message, alone or with text.

A Message that carries a File includes a `[File id=<uuid> ...]` line; the Bot passes that id to `neha file get`. The Bot never prints or stores its token.

Create a small file and send it with a request:

```bash
printf 'a,b\n1,2\n' > rows.csv
target/debug/neha chat send --file rows.csv "Count the rows in the attached file and send me the result as count.csv."
```

The Bot gets `rows.csv` into its Workspace, writes `count.csv`, and sends it back with a Bot Message. The Chat then holds both Files; `neha chat files` lists them and `neha file get <file-id> --out count.csv` downloads the result.

The shell commands go through the same approval gate as any other shell command, so a first use or an Ask rule for `shell` covers them. There is no separate File action to approve.

## Limits

An action that breaks a limit returns an error to the Model instead of failing the whole run.

- A File is at most 32 MiB. A larger file is refused before anything is recorded: the CLI stops with an error, the browser reports that the file was not sent, and a Bot's `chat send --file` fails.
- A Bot reaches only the Files of its own Chat. A File of another Chat is not found.
- `file get` without `--out` writes inside the Bot's Workspace, whatever the shell's current directory; `--out` and `chat send --file` use paths the shell can already reach, so the shell's own approval and sandbox rules apply. A Hosted shell reads the machine's system directories and its own Workspace, and writes only the Workspace and the sandbox's own temporary folders. A Local shell reads with the machine user's rights, except the Computer's configuration directory in granted mode, and writes only where its Workspace, its granted folders, the run's `--cwd` directory, or full access allow; [Add a Local Computer](computer.md#the-bots-workspace-and-the-write-fence) covers that range.
