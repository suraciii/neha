# Message

## Authority

Owned by messaging. A Message depends on [Space](../../space/product.md) to hold its Files, and on [User](../../../CONTEXT.md) and [Bot](../../bot/product.md) as its authors. [Computer](../../computer/product.md) gives a Bot the Workspace it moves Files through.

## Laws

1. Every Message has an author, set from a verified identity.
2. A Message keeps its place in the stream for as long as the record exists.
3. A Card's data matches its schema at the moment it is written.

## Content

A Message carries text, and it can carry a File or a Card.

| Kind | A user sees | A program reads |
| --- | --- | --- |
| Text | The text | The text |
| Text with a File | The text and the file | The text and the File's id, name, type, and size |
| Card | A rendered card | The Card's data, valid against its schema |

A Card has a kind. Neha provides four kinds, and a Plugin can add more.

| Kind | What it holds | What the user does with it |
| --- | --- | --- |
| Approval | An action and its details | Allows, always allows, or denies |
| Question | A question and its options | Answers |
| Page | A document or report | Opens it |
| Dashboard | Live figures | Reads them |

An Approval card names one action of one run. It shows the action's tool, a summary, why the Bot asks, and whether the action can be undone: `shell` or `write_file` is not undoable, and every other action is unknown.

An ambiguous form gives the reason `a form no rule can judge`, not a rule the User set.

The summary escapes control characters and says how many bytes it leaves out when it is longer than 4096 bytes.

When a rule asks and the action can make a lasting rule, the card shows what Always allow covers: the shell prefix, the file directory, the Workspace root, or the whole tool.

It offers Allow, Always allow, and Deny; the first use of a Local Computer shows no Always allow.

After first use, an approval for a folder the owner has not granted offers Always allow, which grants that folder for later runs, only when a grant can store the folder. [Bot design](../../bot/design.md#approvals) states which folders a grant can store. That reason also names the first action of a Local Computer, whose card shows no Always allow.

A rule-triggered approval also offers no Always allow when the action cannot make a lasting rule: a path with a `..` or all-whitespace component, a shell command with no first word, or a file action with no usable path.

An ambiguous form still offers Always allow when the action can make a lasting rule, and that answer removes no rule.

## Files

A Message carries at most one File. A Message names the File by its id, and the record keeps the File's name, type, and size.

A File belongs to the Chat or Group it was sent in. The Files of a Chat or Group form one list, in the order the Space recorded them. [Search](../search/product.md) covers finding a File by its name.

A File enters the conversation when the Space stores its bytes. A Message can carry it, and a File that no Message carries stays in the list. [Space](../../space/product.md) covers the bytes.

A File lives in the Space. A Message with a File reads the same from every client, and any client that reads the Message downloads the same File.

A File is readable wherever a Message carrying it is readable.

A File moves in three ways:

| Who | Move | What happens |
| --- | --- | --- |
| A User | Sends | A Message carries one File, with or without text |
| A Bot | Delivers | A Message from the Bot carries a file of its Workspace as a File |
| A Bot | Takes | The Space writes a copy of a File of the conversation into the Bot's Workspace under the File's name. The File stays in the conversation, and a File of another conversation is refused. |

A File is at most 32 MiB. The Space refuses a larger file, tells the sender, and records no File and no Message.

## Threads

A reply points at the root of its Thread. The Message it answers is noted beside the root.

A Thread has one level: a reply to a reply points at the root.

Under a Task, the Thread is the work log.

## Edits and deletion

An author edits its own Message. The edit is marked, and the record keeps the earlier text.

An author deletes its own Message, and a Bot's owner deletes the Bot's Messages. The content is hidden, the Message keeps its place, and its Thread stays.

An edit or a deletion appears on every client and every surface that carries the Message.

## Authors

A User writes as itself. A Bot writes as itself, and the record notes the User it acted under.

The Space sets the author from the verified identity; what the writer claims never sets it.
