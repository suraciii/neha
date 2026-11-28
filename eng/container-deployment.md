# Container deployment

This page states the target shape for a containerized **Hosted Space**. It is an engineering design; [Computer](../specs/computer/design.md) owns the Computer contract.

## Scope

- A **Hosted Space** runs in one Space container behind a TLS proxy. Its Hosted Computers run in worker containers on the same host.
- A **Local Space** remains the desktop app's native, non-containerized deployment.
- A **Local Computer** remains a process on a machine its User controls. Remote Local Computers are a separate capability.
- PostgreSQL, a separate web server, and Kubernetes are out of scope. The Space keeps its SQLite record and one Rust process.

## Topology

```text
Internet
   │ 443
┌──▼──────────── Host ──────────────────────────────────────┐
│ Caddy (only published ports)                              │
│   │ 127.0.0.1:7337, shared network namespace              │
│ ┌─▼────────────────────┐                                  │
│ │ Space container      │                                  │
│ │ record, API, UI      │──► Model providers                │
│ │ Bot loop, push       │                                  │
│ │ no bwrap, no Docker  │                                  │
│ └─┬──────────────────▲─┘                                  │
│   │ internal HTTP     │ wss, worker connects out            │
│ ┌─▼──────────────┐    │                                   │
│ │ Supervisor     │    │                                   │
│ │ Docker API     │    │                                   │
│ └─┬──────────────┘    │                                   │
│   │ creates           │                                   │
│ ┌─▼──────────────┐    │                                   │
│ │ Computer worker│────┘                                   │
│ │ container      │                                        │
│ │ /workspace     │                                        │
│ └────────────────┘                                        │
└───────────────────────────────────────────────────────────┘
```

The Space and Caddy share one network namespace because the Space binds its loopback listener. Only Caddy publishes a host port. Each Computer worker has no public port; it opens an outbound channel to the Space's origin.

## Processes

| Process | Responsibility | Must not have |
| --- | --- | --- |
| Space | Record, command surface, UI, Bot loop, Model calls, approvals, notifications | bwrap, Docker socket, Hosted Computer Workspaces |
| Caddy | TLS and the public origin | Computer access |
| Supervisor | Create, start, stop, reset, and remove worker containers; inject the Computer token | Record, Vault, or provider keys |
| Computer worker | Run one Hosted Computer's tools, files, and shell; answer the Computer channel | The record, the Vault, the Space home, or another Computer's Workspace |

The Supervisor is a control-plane process, not a Computer. A worker is a Computer: it connects out to the Space over the Computer channel and listens on no network port.

## Storage

The Space home stays one directory on a host bind mount:

```text
data/
  db/                  SQLite record
  secrets/             sealed Vault and provider keys
  blobs/               local File store, when used
  state/               private Space state
  workspaces/<computer-id>/        a Hosted Computer's Workspace root
    <bot-id>/                      one Bot's directory
```

Each worker mounts only `workspaces/<computer-id>` at `/workspace`. It mounts no other part of the Space home. The Bots assigned to one Hosted Computer share that mount, each working in its own `<bot-id>` directory, so several Bots can share one Computer and its worker. A backup of the data directory therefore contains the record, the secrets, and every Hosted Computer's Workspace root with each Bot's directory.

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

- The Space binds `127.0.0.1` and shares a network namespace with Caddy.
- Only Caddy publishes ports 80 and 443.
- A worker reaches the Space through the public origin, or an internal TLS origin when the deployment provides one.
- The Supervisor listens on an internal network only.
- Worker egress defaults to public destinations; LAN, the Docker host, and cloud metadata addresses are refused unless an operator opens them.

## Lifecycle

A new Hosted Computer is provisioned in this order:

1. The Space creates the Computer id and a one-use token.
2. The Supervisor creates a worker container with the Space origin, the token, and the Computer's Workspace root mount.
3. The Space records the Hosted Computer and any Bot it was created for.
4. The Supervisor starts the worker; the worker connects out and the Space marks the Computer online.

A failure before step 4 removes the worker and leaves no recorded Bot without a Computer. A failure after step 3 leaves an offline Computer that the Supervisor restarts.

A worker that stops reconnects while it runs. The Space marks the Computer offline when its channel is down. A run already in flight follows the Computer contract: its actions become unknown and the Space records the failure.

Reset clears every Bot's directory in the Computer's Workspace, and only when the Computer has no queued, running, or waiting run. Removing a Bot removes its directory; the Computer's worker and Workspace root stay while another Bot uses them. Removing the last Bot removes the worker and the Workspace root idempotently.

Space, Supervisor, and worker images ship as one version. A worker with a mismatched channel version refuses to serve; a mixed-version deployment is not supported.

## Single-host deployment

The supported container deployment is one Docker host:

```text
caddy        published 80/443; TLS certificates in a volume
space        network_mode: service:caddy; data bind mount
supervisor   internal network; Docker socket; internal token
workers      created by the Supervisor, not fixed Compose services
```

The images are built in a multi-stage build: the Space image from the Rust binary and the built UI, the worker image from the `neha` binary and its shell tools, and the Supervisor from its own entry point. The Space image runs as a non-root user.

## Non-goals

- Kubernetes or multi-node scheduling.
- A hosted, multi-tenant control plane.
- Hosted Computer browsers or screens.
- Remote Local Computers.
- Provider-backed Hosted Computers.
- Treating a worker container as a trusted boundary between mutually hostile Users.

## References

- [Rakazo self-hosting](https://rakazo.com/docs/self-host.md): API, worker, Postgres, web, and a sandbox supervisor with per-Bot computer containers.
- [OpenHands Docker Compose architecture](https://docs.openhands.dev/enterprise/docker-compose/architecture): Caddy as the only published workload, dynamic unprivileged sandbox containers, and a Docker socket confined to the application and worker.
