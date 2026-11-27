# MCP servers

An MCP server gives a Bot tools that the Space calls for it. The Space speaks
the Streamable HTTP transport of the Model Context Protocol. The tool runs
elsewhere; the Space sends the request, records the answer as the action's
result, and keeps the credential in its own Vault.

## Connect a server

```bash
NEHA_MCP_TOKEN=… target/debug/neha mcp add docs https://docs.example.com/mcp
NEHA_MCP_TOKEN=… target/debug/neha mcp add docs https://docs.example.com/mcp --scope user
target/debug/neha mcp list
```

The name is unique in the Space, and holds letters, digits, and `-` only. The
address is `https`, or `http` on the Space's own loopback.

Only the Admin connects a server for the Space. `--scope user` connects it for
the signed-in User alone: a Bot owner may add one for themselves, or connect an
existing Space server with their own credential. The credential comes from
`NEHA_MCP_TOKEN` only, never from the command line; the Space seals it on the
machine that runs the Space and never puts it in the record or a Computer.

A User-scope server is offered to a Bot only while the Bot acts for that User.
`neha mcp list` prints each server, its scope, its address, and whether a
credential is connected. `neha mcp remove <server>` disconnects one.

## Give the tools to a Bot

```bash
target/debug/neha bot mcp enable <bot-id> <server-id>
target/debug/neha bot mcp disable <bot-id> <server-id>
```

The Bot calls a server's tool as `mcp__<server>__<tool>`. A name the Model could
not take is folded to letters, digits, `_`, and `-`, and a folded or too long
name carries a hash of the server and the tool. A tool the server no longer
offers is not offered to the run.

## Approvals

The first call of one server's tool asks once for each Bot and acting User.
The card names the server and the tool it calls, and the answer is bound to
them: a server whose tool list changes cannot make the answer run another
tool. An Always allow writes a lasting Allow rule for that server's tool alone.

```bash
target/debug/neha rule add --bot <bot-id> --effect ask --tool search --server docs
```

A server's tool rule names the server and the tool's original name, and carries
no prefix. [Approvals](approvals.md#lasting-rules) covers the rest of the rule
commands.

## What reaches the record

An action records the server's id and the tool's original name, never the name
the Model sees, so a rule, a card, and an action name one tool the same way.
Text enters the answer as text, an image as a File in the Chat, and anything
else as JSON text. A server that fails to answer leaves its tools out of the
run and the other servers and skills usable; a call to a server with no
credential answers that it is not connected and sends no request.
