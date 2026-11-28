# Plugins

A plugin package is a directory that follows version 1.0.0 of the Agent
Plugins specification: a `plugin.json` manifest, skills under `skills/`, and
MCP servers in `mcp.json`. Installing one is reading it. Nothing in a package
runs while it is installed, and removing it leaves the record as it was.

## Allow a source

A Space installs a package only from a source its Admin names in
`<home>/config.toml`:

```toml
[plugins]
sources = [
  "https://github.com/acme/",
  "https://codeload.github.com/acme/",
  "/srv/neha-packages/",
]
```

Each entry names a repository URL or a directory the Space may install from,
and covers that source and what is below it: `https://github.com/acme/` allows
any repository under that URL, and `/srv/neha-packages/` allows any package
directory under that path. A GitHub archive redirects to `codeload.github.com`,
so a GitHub source needs both entries; every redirect is checked against the
list.

A directory source is a plain path: `file://` is not a repository scheme, and a
`file://` source is refused. A directory is resolved before it is matched and
then read through its opened handle, so a swapped symlink cannot lead outside
an allowed directory. Directory sources are supported on Linux only; on any
other platform, macOS among them, the Space refuses one with an error, and a
package must come from a repository there.

With no `[plugins]` section, no source is allowed and every install is refused.
The Space restarts with the new list.

## Install

```bash
target/debug/neha plugin install \
  https://github.com/acme/reports@3f2a1c9d8e4b5a6f7c8d9e0f1a2b3c4d5e6f7a8b
target/debug/neha plugin install ./reports-plugin
```

A repository source is `<url>@<commit>`: the commit is required, and names the
immutable object id of the commit to read, 40 or 64 lowercase hexadecimal
characters. A branch or tag is not a commit, and is refused. The Space
downloads `{url}/archive/{commit}.tar.gz`. The commit and the package's digest
are what the record keeps. A directory source is read where it is and gets no
commit: use it for a package you built yourself, not for one to distribute.

The install prints the package's id, name, version, digest, and source, the
commit when it came from a repository, and what it brought: its servers, its
skills, and every part it skipped and why. An invalid `plugin.json` rejects the
whole package. An invalid skill, an invalid server entry, an unsupported
transport, or a name already taken is skipped and the rest of the package
loads.

The Space keeps a package under `<home>/plugins/<name>@<digest>/`. The limits
are 200 files, 100 KiB per file, 1024 entries in all (files and directories),
and 4 MiB for the package; a package above any of them is refused with a
report. Installing the same package again replaces the
record of it with the new digest, and the servers and skills it brought are
replaced with the new package's.

## What a package brings

Each server in `mcp.json` joins the Space's servers under the name
`<plugin-name>-<key>`: a `streamable-http` server with the fixed headers the
package configures, and a `stdio` server as the launch configuration the Space
keeps. A package carries no credential: an HTTP server that configures a header
that could carry one (`Authorization`, `Cookie`, `Set-Cookie`, or a name
holding `auth`, `token`, `key`, `secret`, `password`, or `credential`) is
skipped with a report, and the rest of the package loads. A header the Space
generates (`Accept`, `Content-Type`, `MCP-Session-Id`, the protocol version,
`Host`, `Content-Length`, `Transfer-Encoding`, `Connection`, `Accept-Encoding`,
`User-Agent`, and every other `Proxy-*` header) is dropped with a report so
the Space's value always wins. The rest of the headers are sent with every
request, including discovery.

An HTTP server without a credential is still discovered, so its tools are
offered; a call to an HTTP server made without a credential answers that it is
not connected and sends no request. An Admin attaches a Space-wide token with
`NEHA_MCP_TOKEN=… target/debug/neha mcp add <server-name> <server-url>`, or
signs in from the Integrations view in the browser; once it is attached, the
token reaches the server with the configured headers. The token never comes
from the package.

A `stdio` server is a program on the Space's machine, and the install keeps its
launch configuration as the package wrote it: the command, its arguments, the
environment, and the working directory, with `${PLUGIN_ROOT}` and
`${PLUGIN_DATA}` left for the launch to expand. An entry the record cannot hold
is skipped with a report. A server whose `env` names a variable the Space
reserves (`PLUGIN_ROOT`, `PLUGIN_DATA`, `NEHA_MCP_TOKEN`, `NEHA_HOME`, or
`NEHA_ALLOW_UNSANDBOXED_MCP`) is skipped with a report. The install creates
the package's data directory, `<home>/plugin-data/<plugin-name>/`, which
outlives an update. The Space creates a missing working directory the package
roots under the data directory when it starts the server; a package-rooted one
must already exist. The Space starts the program when it discovers its tools
or calls one, and stops it after: it is not a daemon between calls. It runs in
a bubblewrap sandbox: the system directories and the package are mounted
read-only, the package's data directory is writable, the network is shared,
and no directory of the Space's home is mounted.
`PLUGIN_ROOT` and `PLUGIN_DATA` are set, and a credential attached in the
Integrations view or with `neha mcp add <name>` reaches the process as
`NEHA_MCP_TOKEN`; the package's environment never holds one. A `./` command
that names a package file is published executable. A call needs no credential: the process decides
whether it wants one. A Linux machine without bubblewrap cannot run one;
on Windows one runs unsandboxed only with `NEHA_ALLOW_UNSANDBOXED_MCP=1`, and
any other platform has no sandbox for it, so the install skips the server with
a report instead of registering one that can never run.

Each skill in `skills/` joins the Space's library. Its name, its description,
its body, and its scripts are recorded; its scripts run only as a Bot's
action, after the same approvals a skill you add yourself gets.

A Bot uses a package's servers and skills only when its owner enables them:

```bash
target/debug/neha bot mcp enable <bot-id> <server-id>
target/debug/neha bot skill enable <bot-id> <skill-id>
```

## Manage

```bash
target/debug/neha plugin list
target/debug/neha plugin enable <plugin-id>
target/debug/neha plugin disable <plugin-id>
target/debug/neha plugin remove <plugin-id>
```

`plugin list` prints each package, its digest, its commit, whether the Space
offers it, and the servers and skills it brought. A disabled package's servers
and skills are offered to no run; the record keeps them until the package is
removed. `plugin remove` removes the package, its servers, its skills, and the
credentials stored for those servers, and leaves the record's history as it
was.

A removal can succeed while its cleanup did not finish: the package has left
the record, but its directory or an MCP server's stored credential may remain.
The command reports it with a `Cleanup incomplete: …` line. Running
`neha plugin remove <plugin-id>` again finishes that cleanup even though the
package is already gone: the Space finds the removal its record holds for that
package, completes what it started, and reports `Cleanup finished.`
