# Diagnostics

This page states the rules every diagnostic signal follows. It is the one
authority for the event catalog, the field allowlist, the configuration
variables, and the startup report. Code comments and user guides link here
instead of repeating the rules.

## Signals stay on the machine

A diagnostic signal leaves the process only on stderr. stdout carries the
command's result and the startup lines the desktop and the tests parse, so a
diagnostic never writes there.

The Space sends no telemetry anywhere. No external sink, collector, product
analytics endpoint, or crash service receives a signal. A future sink is a
separate product decision; until then, an operator reads stderr through
journald, Docker, or the desktop shell.

The record stays the source of truth and the User-visible audit. A diagnostic
signal references a record identifier; it never copies a message, a reply, a
prompt, a tool argument or result, a file name or content, a display name, or a
credential.

## Configuration

| Variable | Values | Default | Invalid |
| --- | --- | --- | --- |
| `NEHA_LOG` | level names or `target=level` directives | `info` for `serve` and `computer run`, `warn` for a one-shot command | the start fails before the home opens |
| `NEHA_LOG_FORMAT` | `json` or `text` | `json` | the start fails |
| `NEHA_ENV` | `development`, `staging`, `production`, `test` | `production` | the start fails |

`NEHA_LOG` uses the same directive language as `RUST_LOG`. A plain list of
levels and targets uses a stateless target filter; a directive that names a
span or a field uses the richer filter. Neha reads only `NEHA_LOG`, so a
`RUST_LOG` set for another program cannot change a Space's logging.

The text format exists for a person reading a terminal. It carries the target,
the event name, and the event fields as `key=value` pairs; it omits the per-line
service identity and the trace identifiers. A log pipeline reads the JSON
format.

`NEHA_ENV` also becomes the line's `deployment_environment` field.

## Line shape

Only the Neha targets in the table below are enabled. A dependency's own
events are never written, at any level: a WebSocket or HTTP dependency logs its
requests and messages, including the values Neha sends, and those records would
bypass the scrubber and the catalog.

The JSON format writes one object per line on stderr:

| Key | Type | Meaning |
| --- | --- | --- |
| `timestamp` | string | RFC 3339 in UTC, microsecond precision |
| `level` | string | `TRACE`, `DEBUG`, `INFO`, `WARN`, or `ERROR` |
| `target` | string | stable module name from the target table |
| `event` | string | stable dotted event name from the catalog |
| `message` | string | short sentence for a person; not a contract |
| `service` | string | `neha-space`, `neha-computer`, or `neha-cli` |
| `service_version` | string | the package version |
| `deployment_environment` | string | the `NEHA_ENV` value |
| `openTelemetry` | object | present inside a trace span: lowercase hex `traceId` and `spanId` |
| event fields | any | snake_case values named by the catalog |

The catalog reserves these keys; an event field never repeats one. The
`openTelemetry` key is reserved for the tracing rules; a process that emits no
span omits it.

Event fields follow these types:

- an identifier is a string;
- a duration is `*_ms` as an integer in a log event and seconds in a metric;
- a size is `*_bytes`;
- a classification is a bounded enum such as `outcome`, `status`, `reason`,
  `error_kind`, `phase`, or `storage_kind`;
- an error is a fixed `error_kind` and, where a message is safe, an
  `error_message` from [`scrub_error`](../crates/neha-space/src/diagnostics.rs)
  that is at most 1024 characters; the scrubber removes credentials, quoted
  values, URLs' private parts, and absolute paths;
- a user-controlled value comes from `sanitize_value`, which removes control
  characters and bounds the length.

A field is allowed when this page names it. A new field changes this page and
the code together, and the reviewer checks that it cannot carry content, a
credential, a path, or an unbounded identity.

## Targets

| Target | Code |
| --- | --- |
| `neha::space` | Space lifecycle |
| `neha::http` | HTTP requests, responses, and server errors |
| `neha::ws` | client WebSocket connections |
| `neha::command` | one client command |
| `neha::model` | Provider Model calls |
| `neha::mcp` | MCP server calls |
| `neha::security` | refused requests and sign-in limits |
| `neha::store` | record reads, appends, and recovery |
| `neha::blob` | File bytes storage |
| `neha::catalog` | model catalog refresh |
| `neha::runs` | run execution |
| `neha::computer` | Computer pairing and channel |
| `neha::worker` | worker lifecycle |
| `neha::push` | Web Push delivery |
| `neha::diag` | diagnostics themselves and panics |
| `neha::cli` | CLI progress that is not a diagnostic |

## Event catalog

An event name is a contract; a message is not. Each event carries the
identifiers that connect it to the record, and no content.

| Event | Level | Fields | When |
| --- | --- | --- | --- |
| `diag.start` | debug | `log_format` | the subscriber is installed |
| `diag.shutdown` | debug | — | the process stops |
| `diag.panic` | error | `location`, `thread` | a panic hook runs |
| `space.start` | info | `version`, `schema_version`, `protocol_version`, `addr`, `public_origin`, `storage_kind`, `worker_mode`, `ui` | the Space serves |
| `space.stop` | info | `reason`, `uptime_s` | a stop signal arrives |
| `space.start.failed` | error | `phase`, `error_kind`, `error_message`, and `file` when a report was saved, `write_error` when it could not be, or neither when the home is unknown | the Space cannot start |
| `http.request` | debug | `request_id`, `method`, `route`, `status`, `duration_ms` | one HTTP request finished |
| `http.failed` | error | `request_id`, `status`, `error_kind` | one HTTP request answered 5xx |
| `ws.connect` | debug | `connection_id` | a client WebSocket connected |
| `ws.disconnect` | debug | `connection_id` | a client WebSocket disconnected |
| `command.start` | debug | `command_id` | one command started |
| `command.fail` | warn | `command_id`, `code` | one command was refused |
| `run.start` | info | `run_id`, `bot_id`, `chat_id`, `resumed` | a run starts |
| `run.finish` | info | `run_id`, `bot_id`, `chat_id`, `outcome`, `duration_ms`, and the usage fields when reported | a run ends |
| `model.call` | info or warn | `outcome`, `duration_ms`, `error_kind` on failure, and the token fields on success | one Model call ends |
| `mcp.call` | debug or warn | `method`, `transport`, `outcome`, `error_kind` on failure | one MCP call ends |
| `computer.connected` | debug | `computer_id` | a Computer channel opened |
| `computer.disconnected` | debug | `computer_id` | a Computer channel closed |
| `computer.action` | debug | `bot_id`, `status` | a daemon finished one action |
| `worker.operation` | debug or warn | `computer_id`, `operation`, `outcome` | one Supervisor request ends |
| `push.delivery` | debug | `endpoint_host`, `status`, `outcome` | one push delivery ends |
| `security.reject` | warn | `reason` | the Host/Origin guard refused a request |
| `security.rate_limited` | warn | `operation`, `ip` | sign-in attempts were limited |
| `security.login_failed` | warn | `ip` | a sign-in credential was wrong |
| `store.fail` | error or warn | `operation`, `error_message`, and `run_id` when a run owns it | a record operation fails |
| `blob.fail` | debug | `operation`, `error_kind` | a File store operation fails; the correlated `http.failed` carries the 5xx |
| `catalog.refresh` | warn | `outcome`, `error_kind` | a background refresh fails |
| `run.fail` | error | `run_id`, `error_kind`, `duration_ms`, and `resumed` on a resumed run | a run cannot finish |
| `run.resume.failed` | warn | `run_id`, `error_message`, or `reason` when it is not resumable | a resume cannot be prepared |
| `run.recover.failed` | warn | `run_id`, `error_message` | a recovered run cannot be prepared |
| `computer.reject` | warn | `computer_id`, `version`, `sandbox`, `unsandboxed_allowed`, `reason` | a Computer handshake is refused |
| `computer.pairing.finish.failed` | warn | `computer_id`, `reason`, and `error_message` when identity fails | a pairing claim cannot be finished |
| `computer.pairing.release.failed` | error | `computer_id`, `error_message` | a pairing claim cannot be released |
| `computer.pairing.rollback.failed` | error | `computer_id`, `error_message` | a pairing token cannot be rolled back |
| `computer.pairing.reconcile.failed` | warn | `computer_id`, `reason` | a recorded Computer's claim vanished at start |
| `computer.connect.failed` | warn | `error_message`, `retry_ms` | a Local Computer loses its channel |
| `computer.stop` | info or warn | `reason`, or `error_message` when the signal cannot be watched | the daemon stops |
| `computer.ticket.cleanup.failed` | warn | `error_message` | a used pairing ticket cannot be removed |
| `computer.unsandboxed` | warn | `platform`, `reason` | a Windows build runs without an OS sandbox |
| `worker.disabled` | error or warn | `reason`, `error_kind` | the worker Supervisor cannot be built |
| `worker.fail` | warn | `computer_id`, `operation`, `error_message` | a worker operation fails |
| `push.disabled` | warn | `reason`, `error_kind` | Web Push is off for this start |
| `push.fail` | warn | `endpoint_host`, `status` | a push delivery fails |

The run lifecycle, request correlation, metrics, and the support report add
their events and fields in the pull request that implements them; this page
grows with each capability and never lists an event the code does not emit.

## Correlation

Every HTTP response carries `x-request-id` and a W3C `traceparent`. A caller's
`x-request-id` is kept when it is 1-64 characters of `[A-Za-z0-9._-]`; any
other value is replaced. The `http.request` span takes its parent from an
inbound `traceparent`; the response repeats the current span context.
`traceparent` is lower-case hex and never carries content.

`route` is the router's template (`/api/chats/{chat}/files`) and `unmatched`
when no route matched; the raw path is client-controlled and never logged. An
unusual HTTP method becomes `OTHER`. Exactly one `http.failed` error is
recorded for a 5xx response, inside the request span, with the request id and
the failing component's `error_kind`.

`http.request` is the root of a server span tree. A WebSocket connection, one
command, a run, one Model call, one MCP call, and a worker operation are child
spans of the work that caused them. A run cross-process boundary carries
`traceparent` in the Computer action frame; an MCP HTTP call carries the same
value in the `traceparent` header and in JSON-RPC `params._meta.traceparent`;
an MCP stdio call carries the `_meta` field only. Only `traceparent` crosses a
boundary; a vendor `tracestate` is not forwarded. The receiver starts its span
with that parent, so one trace id spans the Space, its Computer, and the MCP
server. Events that belong to a span are emitted inside it, so their JSON lines
name the same trace and span id as the boundary.

Only boundary spans are created: one per request, connection, command, run,
Model call, action, worker operation, MCP call, or push delivery, never inside
a loop. Access `http.request` is debug; failures are one `http.failed` error
per 5xx response, with the span's ids attached by the JSON formatter.

## Panics

The panic hook replaces the default hook so the payload is never printed. It
records `diag.panic` with the source location and the thread name, then writes
one human line that names the location without copying the payload. A
backtrace follows only when `RUST_BACKTRACE` is set and not `0`. A thread-local
guard stops the hook from recursing when formatting itself panics.

## Startup report

A `neha serve` that fails before it serves writes one private report under
`<NEHA_HOME>/logs`:

- the directory is created with mode 0700 and the file with mode 0600 on Unix;
- the name is `startup-<unix-ms>-<pid>.log`;
- the body is JSON with the build, the schema and protocol version, the
  startup phase, `error_kind`, a typed `error` summary, and a warning that raw
  diagnostics may contain configuration or credential values;
- the summary names the failed step, never the original display text: a
  configuration or remote error can quote a value, a path, or a response;
- stderr carries the same `space.start.failed` event with the file path, or a
  `write_error` when the report cannot be written.

The command keeps the original error for its own exit message. That plain
message is product output, not a diagnostic line, and can name configuration
values; the JSON lines and the report are the allowlisted form.

`report_start_failure` in the CLI is the only writer. The user guide states
where to find the report.

## Privacy allowlist

A diagnostic field never carries:

- a message, reply, prompt, model output, tool argument or result, or file
  name or content;
- a display name, password, invitation secret, bearer token, cookie, header,
  provider key, or sealed secret;
- the content of an MCP stdio server's stderr; the Space records only the
  number of discarded bytes;
- an absolute path outside the diagnostic's own file, except the startup
  report path the user must find;
- a client IP address, except in the authentication and rate-limit events
  that a later pull request adds.

`neha detect`-style support reports are not part of this page; the support
report has its own allowlist and the pull request that adds it updates this
page.

## Tests

A test for a diagnostic asserts the parsed event name, level, and fields, not
the whole line. The default test set covers:

- an accepted and a refused `NEHA_LOG`, `NEHA_LOG_FORMAT`, and `NEHA_ENV`;
- the event names and fields the tests exercise;
- `scrub_error` against a credential, a quoted credential, a cookie, a URL
  with user information and a path, a line break, a control character, and an
  over-long message;
- `sanitize_value` against a line break and an over-long value;
- a real `neha serve` that prints `space.start` and, on Unix, `space.stop` after
  SIGTERM, including a stop sent as soon as the address appears;
- an authenticated CLI exchange at `trace` that writes no dependency record and
  never prints the saved bearer token;
- a refused start that leaves a private report;
- stderr that never contains a password or an invitation secret.

A test that starts a process is bounded and reuses one fixture where it can;
`just ci` measures the gate's cost and a pull request reports any increase over
thirty seconds.
