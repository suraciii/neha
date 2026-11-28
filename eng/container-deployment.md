# Container deployment

This page states how a containerized **Hosted Space** runs. It is an engineering design; [Computer](../specs/computer/design.md) owns the Computer contract and [Space](../specs/space/product.md) owns deployment behavior. The operator guide is [Container deployment](../docs/container.md).

## Scope

- A **Hosted Space** runs in one Space container behind a TLS proxy. Its Hosted Computers run in worker containers on the same host.
- A **Local Space** remains the desktop app's native, non-containerized deployment. It keeps its process-local Hosted Computer runtime and needs `bubblewrap`.
- A **Local Computer** remains a process on a machine its User controls. Remote Local Computers are a separate capability.
- PostgreSQL, a separate web server, and Kubernetes are out of scope. The Space keeps its SQLite record and one Rust process.

## Topology

```text
Internet
   │ 443
┌──▼──────────── Host ───────────────────────────────────────────┐
│ Caddy (only published ports; edge network)                     │
│   │ http://space:7337                                          │
│ ┌─▼────────────────────┐                                       │
│ │ Space container      │──► Model providers, push services     │
│ │ record, API, UI      │                                       │
│ │ Bot loop, push       │                                       │
│ │ no bwrap, no Docker  │                                       │
│ └─┬──────────────────▲─┘                                       │
│   │ internal HTTP     │ wss, worker connects out                 │
│ ┌─▼──────────────┐    │                                        │
│ │ Supervisor     │    │                                        │
│ │ Docker API     │    │                                        │
│ └─┬──────────────┘    │                                        │
│   │ creates           │                                        │
│ ┌─▼──────────────┐    │                                        │
│ │ Computer worker│────┘                                        │
│ │ container      │                                             │
│ │ /workspace     │                                             │
│ └────────────────┘                                             │
└────────────────────────────────────────────────────────────────┘
```

Only Caddy publishes host ports. The Space binds `0.0.0.0:7337` on a Docker network that is not published to the host; Caddy and workers reach it there. Each Computer worker has no public port; it opens an outbound Computer channel to the Space on its private network and presents the public `Host` the Space's request guard expects.

## Processes

| Process | Responsibility | Must not have |
| --- | --- | --- |
| Space | Record, command surface, UI, Bot loop, Model calls, approvals, notifications, worker lifecycle | bwrap, Docker socket, a public port |
| Caddy | TLS and the public origin | Computer access |
| Supervisor | Create, start, stop, and remove worker containers; inject the Computer token and Workspace mount | Record, Vault, or provider keys; a public port |
| Computer worker | Run one Hosted Computer's tools, files, and shell; answer the Computer channel | The record, the Vault, the Space home, another Computer's Workspace, or a network listener |

The Supervisor is a control-plane process, not a Computer. A worker is a Computer: it connects out to the Space over the Computer channel and listens on no network port.

The Space creates a worker from the record, not the other way round. A Hosted Computer exists in the record before its worker; a provision failure leaves the Computer recorded and offline, and the lifecycle manager retries.

## Storage

The Space home stays one directory on a host bind mount:

```text
data/
  db/                  SQLite record
  secrets/             sealed Vault and provider keys
  blobs/               local File store, when used
  state/               private Space state
  state/workers/       one Computer token per worker, mode 0600
  workspaces/<computer-id>/        a Hosted Computer's Workspace root
    <bot-id>/                      one Bot's directory
```

Each worker mounts only `workspaces/<computer-id>` at `/workspace`. It mounts no other part of the Space home. The Bots assigned to one Hosted Computer share that mount, each working in its own `<bot-id>` directory, so several Bots can share one Computer and its worker. A backup of the data directory therefore contains the record, the secrets, and every Hosted Computer's Workspace root with each Bot's directory.

The Supervisor receives the Workspace path relative to the deployment data directory. It mounts only `data/workspaces` read-only at `/workspaces`, resolves the real directory there (so a symlink cannot leave the Workspace tree), and maps the verified relative path onto its `NEHA_HOST_DATA_DIR` to produce the Docker bind source. It never receives a path from the Space as an arbitrary host path, and it never mounts the record, Vault, File store, or token state.

SQLite stays on a local filesystem. Do not place the data directory on NFS, SMB, or a network filesystem.

## Worker security

A worker container runs with:

- a non-root user;
- a read-only root filesystem;
- its own `/tmp` as a tmpfs;
- no Linux capabilities and no privilege escalation;
- CPU, memory, and process limits;
- no public port and no host mounts outside the Computer's Workspace root;
- the Computer token, which admits only its channel.

The container is the Computer's isolation boundary. The worker starts no nested sandbox. A compromised Bot can reach its Computer's Workspace and its short-lived Bot credential, but not the Space's record, Vault, File store, or another Computer. Bots assigned to one Computer share its worker and are not isolated from each other.

Workers share the host kernel. A deployment that serves untrusted Users on the same host needs a stronger boundary, such as a VM-per-worker or a gVisor/Kata sandbox, and is outside this design.

## Supervisor trust

The Supervisor needs the Docker API and therefore has host-root authority. It runs on an internal network, accepts only a token-protected internal API, and publishes no port. The Space never mounts the Docker socket.

The whole stack belongs on a host dedicated to Neha. A host that also runs untrusted workloads is not a safe place for this Supervisor.

## Network

- Caddy publishes ports 80 and 443 and joins the edge, internal, and worker networks.
- The Space joins the internal and worker networks and publishes no port.
- The Supervisor joins the internal and worker networks and publishes no port.
- A worker joins one network: the internal network when egress is `restricted`, or the worker bridge when egress is `auto` or `open`.
- Caddy reaches the Space at `space:7337`; a worker reaches it at the same address over its private network and sends the public `Host` header.
- The Space keeps no HTTP proxy setting for its own outbound calls; model providers and push services are reached directly.

Worker egress is one of three modes:

| Mode | Network | What the worker can reach |
| --- | --- | --- |
| `auto` (default) | worker bridge | The Space and the internet through the host's normal bridge egress. The bridge does not filter LAN, host, or metadata destinations. |
| `restricted` | internal network | Only the Space and other containers on the internal network; no internet. |
| `open` | worker bridge | The same route as `auto`; named separately for an operator who wants the choice to be explicit. |

The `auto` mode is a development and first-release default, not a per-destination firewall. A hardened deployment that must refuse LAN and cloud metadata destinations is a future egress-filter capability; do not claim it from this mode.

## Lifecycle

A new Hosted Computer is provisioned in this order:

1. The Space creates the Computer's Workspace root and records the Computer and any Bot it was created for.
2. The lifecycle manager reads or creates one Computer token in `state/workers/` and records it in identity with `INSERT OR IGNORE`.
3. The manager asks the Supervisor to ensure a worker container with the relative Workspace path and the token. The container is created stopped.
4. When the Computer has queued, running, or waiting work, or when `NEHA_WORKER_ALWAYS_ON` is set, the manager starts the worker.
5. The worker connects out, greets with its Computer id, and the Space marks the Computer online. A wake-up already waiting then starts.

The manager polls the record and the run queue once a second. A run on a worker-backed Hosted Computer waits while the Computer is offline, exactly as a run waits for an offline Local Computer. Starting a worker does not block the run queue: the queued run starts when the channel connects and the normal pump wake-up runs.

A worker with no queued, running, or waiting run is stopped after `NEHA_WORKER_IDLE_SECONDS` (default 600). `NEHA_WORKER_ALWAYS_ON=1` keeps it running. Work that arrives after a stop starts it again. A background process does not keep a Computer awake: it runs inside a tool call that the Space's action deadline bounds, and the lifecycle decision follows the run, not the process.

A failure before step 3 leaves an offline Computer that the manager retries. A failure after step 3 leaves an offline Computer with a token and a stopped or unreachable container; the manager retries the start when work arrives. A space restart reuses the token file and asks the Supervisor to ensure the same container, so an existing worker is adopted rather than duplicated.

Reset clears every Bot's directory in the Computer's Workspace, and only when the Computer has no queued, running, or waiting run. The worker may be running but idle during the clear; it opens files per run, so the reset races no open file. Removing a Bot removes its directory when that removal is implemented; the worker and Workspace root stay while another Bot uses them. Removing a Computer, and removing the last Bot's worker and Workspace root, are not implemented yet.

The Supervisor does not delete orphan containers on its own. An operator who deletes the Space's data directory must remove `neha-worker-*` containers explicitly; the guide says how.

Space, Supervisor, and worker images ship as one version. A worker with a mismatched channel version refuses to serve; a mixed-version deployment is not supported.

## Images and Compose

The deployment is one Docker host with fixed Compose services and dynamic workers:

```text
caddy        published 80/443; TLS certificates in a volume; edge + internal + workers
space        no published port; internal + workers; data bind mount
supervisor   no published port; internal + workers; Docker socket; data bind mount
workers      created by the Supervisor, not fixed Compose services
```

The images are built from one checkout:

- `deploy/space.Dockerfile` builds the Rust binary and the browser client; the Space image runs as UID 10001.
- `deploy/worker.Dockerfile` builds the same Rust binary and the worker's shell tools; the worker image runs as UID 10001.
- `deploy/supervisor/Dockerfile` packages the Python Supervisor.

Images are built locally for the first deployment. Publishing them to a registry is a release step; the names in `deploy/.env.example` are local tags.

The Supervisor's Docker API client uses `/var/run/docker.sock` and API version `v1.43`. It validates Computer ids, requires a Workspace path under the deployment data directory, and returns only after Docker accepted the operation.

## Non-goals

- Kubernetes or multi-node scheduling.
- A hosted, multi-tenant control plane.
- Hosted Computer browsers or screens.
- Remote Local Computers.
- Provider-backed Hosted Computers.
- Treating a worker container as a trusted boundary between mutually hostile Users.
- A per-destination worker egress firewall.

## References

- [Rakazo self-hosting](https://rakazo.com/docs/self-host.md): API, worker, Postgres, web, and a sandbox supervisor with per-Bot computer containers.
- [OpenHands Docker Compose architecture](https://docs.openhands.dev/enterprise/docker-compose/architecture): Caddy as the only published workload, dynamic unprivileged sandbox containers, and a Docker socket confined to the application and worker.
