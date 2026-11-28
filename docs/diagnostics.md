# Diagnostics

Neha writes structured diagnostics for the person running a Space and for the
support report they may paste into an issue. The diagnostics stay on the
machine and carry identifiers and outcomes, never the messages, prompts, or
Files a Bot handles.

## Logs

`neha serve`, `neha computer run`, and the one-shot commands write one JSON
object per line to stderr. stdout keeps the command's result and the startup
lines the desktop and the tests read, so a diagnostic never appears there.

A Space usually runs under systemd or Docker, and that manager collects stderr:

```sh
journalctl -u neha --user -o cat | jq .
docker compose logs -f space
```

Only Neha's own events are written. A dependency's own request, frame, or
message log is never enabled, even at `trace`, because those records can carry
the values Neha sends.

The default level is `info` for `neha serve` and `neha computer run`, and
`warn` for a one-shot command, so a normal command stays quiet. `NEHA_LOG`
raises or lowers the level:

```sh
NEHA_LOG=debug neha serve
NEHA_LOG=info,neha::http=debug neha serve
NEHA_LOG=off neha serve
```

A malformed value stops the start before the Space opens its home, with an
error that names the bad directive.

| Variable | Values | Default |
| --- | --- | --- |
| `NEHA_LOG` | a level, or `target=level` directives separated by commas | `info` for a daemon, `warn` for a one-shot command |
| `NEHA_LOG_FORMAT` | `json` or `text` | `json` |
| `NEHA_ENV` | `development`, `staging`, `production`, or `test` | `production` |

`NEHA_LOG_FORMAT=text` prints the target, event name, and event fields for a
person reading a terminal. The JSON format adds the per-line service identity
and trace identifiers, and is the one a log pipeline reads.

## Read one line

```json
{"timestamp":"2026-11-28T21:07:55.123456Z","level":"INFO","target":"neha::space","event":"space.start","message":"the Space is serving","service":"neha-space","service_version":"0.1.0","deployment_environment":"production","version":"0.1.0","schema_version":6,"protocol_version":3,"addr":"127.0.0.1:7337","public_origin":"","storage_kind":"local","worker_mode":false,"ui":false}
```

The `event` name is the stable part. The `message` is for a person and may
change. A line inside a trace span also carries
`openTelemetry.traceId` and `openTelemetry.spanId`.

A panic replaces the raw panic line with a `diag.panic` event and one human
line that names the location; the panic payload is not copied. A backtrace
follows only when `RUST_BACKTRACE` asks for one.

Every HTTP response carries `x-request-id` and a W3C `traceparent`; a valid
caller-supplied `x-request-id` is kept. Set
`NEHA_LOG=debug` to see one `http.request` line per request; a 5xx answer
records exactly one `http.failed` error with the matched route template, not
the raw path. The same trace id follows one run into its
Computer and MCP server calls, so `grep` for the id connects the lines.

`eng/diagnostics.md` lists every event and field. Start with these:

| Event | Meaning |
| --- | --- |
| `space.start` | the Space serves, with the build, schema, address, and storage kind |
| `space.stop` | a stop signal arrived, with the reason and uptime |
| `space.start.failed` | the Space could not start; the line names the phase and the report |
| `diag.panic` | a thread panicked, with the source location |
| `run.fail` | a run could not finish |
| `computer.connect.failed` | a Local Computer lost its channel and reconnects |

## Metrics

`GET /metrics` is closed by default (404). Set a token of 32 or more
characters with no whitespace to enable it:

```sh
export NEHA_METRICS_TOKEN="$(openssl rand -hex 32)"   # 64 characters
neha serve &                                         # listens on 127.0.0.1:7337
curl -H "Authorization: Bearer $NEHA_METRICS_TOKEN" http://127.0.0.1:7337/metrics
```

The response is Prometheus text (`text/plain; version=0.0.4`). A missing or
wrong token answers 401; a malformed token keeps the endpoint closed and logs
`metrics.disabled` at start. Do not expose the endpoint without a proxy rule
that limits who can reach it.

Useful queries:

```promql
# Run failure rate over five minutes
sum(rate(neha_runs_finished_total{status="failed"}[5m]))
  / sum(rate(neha_runs_finished_total[5m])) > 0.2

# Model retries
sum(rate(neha_model_retries_total[5m])) > 1

# Queue backlog
max_over_time(neha_runs_queued[10m]) > 5

# A Computer is offline while work waits
neha_computers_connected < 1 and neha_runs_queued > 0

# Record growth over an hour
delta(neha_record_bytes[1h]) > 1e9
```

`GET /readyz` answers 200 `{"status":"ready"}` while the Space accepts new
work and 503 `{"status":"shutting_down"}` once shutdown starts. It reports
whether new work is accepted, not whether a dependency is healthy; `/healthz`
remains the liveness check.

## A failed start

A `neha serve` that fails before it serves writes one private report under the
Space's home:

```text
<NEHA_HOME>/logs/startup-<unix-ms>-<pid>.log
```

The directory is readable only by the user and on Unix the file is mode 0600.
The report contains the build, the schema and protocol version, the start
phase, an error kind, and a typed summary of the failure. The summary names the
failed step and leaves out the original error text, which can quote a
configuration value, a path, or a provider response. The same failure prints
one `space.start.failed` line with the report path; when the report cannot be
written, that line carries a `write_error` instead.

The command's own last line is its ordinary error message, not a diagnostic
line, and can name a configuration value. The JSON lines and the report are the
allowlisted form.

The report can still name the build, a provider, or a bucket through the
summary warning; review it before you paste it into an issue.

## What is never written

- message text, model replies, prompts, tool arguments or results;
- File names or File content;
- display names, passwords, invitation secrets, bearer tokens, cookies,
  headers, or provider keys;
- the content of an MCP stdio server's stderr;
- an audit trail of sign-ins or approvals; the record is the User-visible
  history, and diagnostics are not a compliance audit.

## Troubleshooting

| Symptom | Check |
| --- | --- |
| no logs at all | `NEHA_LOG` is `off` or higher than the event's level; a one-shot command defaults to `warn` |
| logs are text | `NEHA_LOG_FORMAT=text` is set; unset it for JSON |
| a start fails with no reason | read `space.start.failed` on stderr and the report it names under `<NEHA_HOME>/logs` |
| a service manager shows no stderr | the unit or compose service captures stderr; `journalctl` or `docker compose logs` shows it |
| a report is unreadable | the file is owned by the user that ran `neha`; on Unix the directory is 0700 |

## Related

- [Serve a Space on a domain](deploying.md)
- [Run a Space in containers](container.md)
- [Computers](computer.md)
