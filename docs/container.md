# Run a Space in containers

This guide deploys a **Hosted Space** with Docker Compose on one Linux host. Users reach it through Caddy on a public HTTPS domain; the Space and its Hosted Computers run in containers that publish no port.

A **Local Space** is different: the desktop app starts it on your computer and it keeps its Hosted Computers in its own process. Use this guide when you want a server that other people reach over the network.

## What runs

| Service | What it does | Reachable from |
| --- | --- | --- |
| Init | Creates and owns `data/workspaces` once; exits | Nothing |
| Caddy | TLS and the public origin | The internet on ports 80 and 443 |
| Space | The record, API, browser client, Bot loop, notifications, worker lifecycle | Caddy and workers on private Docker networks |
| Supervisor | Creates, starts, and stops worker containers through the Docker API | The Space and workers on private networks |
| Computer worker | One Hosted Computer's files, tools, and shell; connects out to the Space | No network listener |

The Space creates one worker container per Hosted Computer. A Computer's Bots share its worker and its Workspace, each in a directory of its own. The worker mounts only that Computer's Workspace root at `/workspace`.

The Space trusts the `caddy` service as its TLS proxy (`NEHA_TRUSTED_PROXIES=caddy` in `compose.yml`), so sign-in limits count each client's forwarded address. Caddy shares only the internal network with the Space and has a fixed address there, outside the dynamic range; a proxy container recreated on its own does not leave the Space naming a stale address. Restart the Space if you replace the proxy outside this stack.

## Requirements

- A Linux host with Docker Engine and the Compose plugin.
- A DNS name whose A or AAAA record points at the host.
- Ports 80 and 443 reachable from the internet for Caddy's certificate and traffic.
- Enough disk for the data directory. SQLite and the Workspaces stay on the host filesystem; do not put them on NFS, SMB, or another network filesystem.

The Compose stack belongs on a host dedicated to Neha. The Supervisor controls Docker and therefore has host-root authority.

## Build the images

From the repository root:

```bash
docker build -f deploy/space.Dockerfile -t neha-space:dev .
docker build -f deploy/worker.Dockerfile -t neha-worker:dev .
docker build -f deploy/supervisor/Dockerfile -t neha-supervisor:dev deploy/supervisor
```

The Space and worker images run as UID 10001. The build needs network access for the Rust and browser dependencies. The first build takes several minutes; later builds reuse the layer cache.

The tags are local names. A release can publish the same images under registry names and change `NEHA_WORKER_IMAGE` and the `build:` entries in `deploy/compose.yml`; this guide uses local images.

## Configure the deployment

```bash
cd deploy
cp .env.example .env
```

Edit `.env`:

| Setting | What to set |
| --- | --- |
| `NEHA_DOMAIN` | The public DNS name, such as `neha.example.com`, with no scheme or path |
| `NEHA_HOST_DATA_DIR` | The absolute path to `deploy/data` on the Docker host, such as `/srv/neha/data` |
| `NEHA_SUPERVISOR_TOKEN` | A long random secret shared by the Space and Supervisor |
| `NEHA_WORKER_IMAGE` | The worker image tag you built, `neha-worker:dev` for a local build |
| `NEHA_WORKER_ALWAYS_ON` | `0` to wake a worker when it has work; `1` to keep every worker running |
| `NEHA_WORKER_IDLE_SECONDS` | How long a worker with no work stays running; the default is 600 seconds |
| `NEHA_WORKER_EGRESS` | `auto`, `restricted`, or `open`; [Worker egress](#worker-egress) explains the tradeoff |
| `NEHA_LOG` | The JSON diagnostics level; `info` by default, `debug` for more detail |
| `NEHA_LOG_FORMAT` | `json` by default; `text` for a person reading the container output |
| `NEHA_ENV` | The `deployment_environment` value on each line; `production` by default |

Change `deploy/Caddyfile` if the public host is not the `NEHA_DOMAIN` value; the file uses `{$NEHA_DOMAIN:localhost}`.

Create the data directory and give it to the Space and worker user:

```bash
mkdir -p data/workspaces
sudo chown -R 10001:10001 data
```

The one-shot `init` service repeats the directory and ownership setup before the Space and Supervisor start. It repairs a fresh host where Docker created `data/workspaces` as root, and it is why a first `up` does not need a second one.

The Supervisor mounts only `data/workspaces` at `/workspaces`, read-only. It resolves and verifies the Computer's Workspace there, then maps the verified relative path onto the host path in `NEHA_HOST_DATA_DIR` for Docker. It cannot read the record, the sealed Vault, the File store, or a worker token. It runs as root only because the Docker socket needs that authority.

## Start the stack

```bash
docker compose up -d
docker compose logs -f space
```

The Space prints its invitation link when it has no User yet:

```text
Open in a browser: https://neha.example.com/invite#<secret>
```

Open it, create the Space Owner account, and connect a Provider. A Provider is needed before a Bot can answer. [Quick start](quickstart.md) covers the first Chat.

If the Space has a User already, the log prints the sign-in address instead. The first start can take a few seconds while SQLite and the browser client are prepared.

Check the public origin:

```bash
curl --fail https://neha.example.com/healthz
```

The answer is `ok`. Caddy obtains the certificate on the first request for the domain. Make sure DNS already points at the host and ports 80 and 443 are open.

## Hosted Computers

A new Bot starts on its owner's Space Computer, a Hosted Computer the Space creates on first use. The Space records the Computer and then asks the Supervisor to create a worker. The worker stays stopped until the Computer has work or `NEHA_WORKER_ALWAYS_ON=1` is set.

Send a Message to a Bot and the first run wakes the Computer:

1. The Space records the run as queued.
2. The lifecycle manager starts the worker.
3. The worker connects to the Space on its private network.
4. The queued run starts; its shell and file tools run in the worker.
5. After `NEHA_WORKER_IDLE_SECONDS` with no queued, running, or waiting run, the manager stops the worker again.

A worker that has stopped starts again on the next Message. A run already in flight when its Computer goes offline is recorded as failed or unknown, as the Computer contract says.

The worker's shell starts in the Bot's own directory under `/workspace`, and the whole Computer Workspace is readable and writable, so Bots on one Computer share files. A worker's `/tmp` is temporary and disappears when the container stops.

A Hosted Computer has no screen, browser, Vault credential, or Take control yet.

## Worker egress

`NEHA_WORKER_EGRESS` chooses the worker's Docker network:

| Value | What a worker can reach |
| --- | --- |
| `auto` | The Space and the internet through the host's normal bridge route. This is the default. |
| `restricted` | Only the Space and other containers on the internal network. No internet. |
| `open` | The same route as `auto`; use it when the operator wants the choice to be explicit. |

`auto` does not filter LAN, the Docker host, or cloud metadata destinations. A deployment that must refuse those destinations needs a stronger network design than this release provides; do not treat `auto` as that firewall. `restricted` gives the Bot no internet but still lets it reach the Space.

Changing the mode does not move an existing worker. Remove the `neha-worker-*` container for that Computer and let the lifecycle manager create it again, or recreate the stack.

## Data and backups

Every durable part of the deployment is under `deploy/data`:

```text
data/
  db/                 the SQLite record
  secrets/            the sealed Vault and Provider keys
  blobs/              local File bytes, when the Space uses local storage
  state/workers/      one worker token per Computer, mode 0600
  workspaces/<computer-id>/<bot-id>/   each Bot's files
```

Stop the stack before copying the directory, or copy it while it is stopped; SQLite must not be copied while it is being written:

```bash
docker compose stop space supervisor
sudo tar -C data -czf /safe/neha-data.tgz .
docker compose start supervisor space
```

Restore by stopping the stack, replacing the contents of `data`, restoring ownership to UID 10001, and starting it again. A worker token file lets a restarted Space give the same token to an existing worker. If the file is missing, the Space records a new token; a token an existing worker container still holds stays valid, and a newly created container uses the new token. Keep the file in the backup so a restart and a recreated worker agree.

Back up the Caddy volumes too if you want to keep certificates across a rebuild:

```bash
docker compose stop caddy
docker run --rm -v neha_caddy_data:/data -v "$PWD":/backup alpine \
  tar -C /data -czf /backup/caddy-data.tgz .
docker compose start caddy
```

## Upgrade

Build the new images first:

```bash
docker build -f deploy/space.Dockerfile -t neha-space:dev .
docker build -f deploy/worker.Dockerfile -t neha-worker:dev .
docker build -f deploy/supervisor/Dockerfile -t neha-supervisor:dev deploy/supervisor
cd deploy
# The fixed services use the tags named in compose.yml. `up` alone does not
# rebuild an image from a changed Dockerfile; `build` does.
docker compose build space supervisor
```

### A stack created before the fixed proxy address

This release changes the internal network and the proxy's address. Migrate before the first `up`: a running worker holds the old network, and a stopped old worker must not be left to start against it.

```bash
# 1. Stop the Compose services. Host data and named Caddy volumes are kept.
docker compose down
# 2. Remove this deployment's workers, including stopped ones.
docker ps -aq --filter label=neha.worker=1 --filter name=neha-worker- | xargs -r docker rm -f
# 3. Remove the network the old workers held.
docker network rm neha-internal
# 4. Start the new stack.
docker compose up -d
```

If Compose reports that `neha-internal` is still in use, list the containers on it (`docker ps -a --filter network=neha-internal`) and remove the remaining `neha-worker-*` container before repeating the network removal. If `172.28.0.0/24` collides with a network on your Docker host, change the subnet and Caddy's address in `compose.yml` before the first start; keep Caddy's address outside the `ip_range`.

### Every upgrade

```bash
docker compose up -d --force-recreate space supervisor caddy
```

Workers are not Compose services, so `up` does not replace them. Remove the workers that still use the old image; the lifecycle manager creates new ones from the new image for Computers that need them:

```bash
docker ps -aq --filter label=neha.worker=1 --filter name=neha-worker- | xargs -r docker rm -f
```

A worker running an old channel version refuses to serve, so a mixed-version stack does not answer as if it worked. A worker that is still running keeps its old container even when it is idle; remove it so the next start uses the new image.

Back up the data directory before an upgrade. The schema migration runs when the Space starts; [Upgrade a Space](upgrading.md) covers the version check and rollback expectations.

## Troubleshooting

| Symptom | What to check |
| --- | --- |
| The invitation link does not open | DNS points at the host, ports 80 and 443 are open, and `docker compose logs caddy` shows a certificate. |
| The Space exits at start | `data` must be writable by UID 10001; check its ownership. A `space.start.failed` line names the failing phase and the private report under `data/logs`; [Diagnostics](diagnostics.md) covers the report. |
| A Hosted Computer stays offline | `docker compose logs supervisor` shows the Docker API error. A missing worker image, an unavailable Docker socket, or a Workspace that does not resolve under `/workspaces` appears there. A wrong `NEHA_HOST_DATA_DIR` lets Docker create the missing host directory, so look for worker permission errors and check that the value is the absolute `deploy/data` path. |
| A worker starts and immediately exits | `docker compose logs <worker container>`; check the Space URL and the public `Host` value the Supervisor injected. |
| A Bot cannot install a system package | The worker root filesystem is read-only. The Workspace is writable; the operating system is not. |
| A Bot needs internet but `restricted` is set | Use `auto`, and accept that `auto` has the bridge's LAN and metadata reach. |

The Space, Supervisor, and worker logs are the primary evidence. All three are ordinary Docker logs.

## Clean up orphan workers

The Supervisor creates containers named `neha-worker-<computer-id>` and does not delete them when the record is lost. If you delete or replace `data` without removing workers, list and remove the leftovers:

```bash
docker ps -a --filter label=neha.worker=1 --format '{{.Names}}'
docker rm -f <name>
```

The lifecycle manager can then create a fresh worker for each recorded Computer.

## Security limits

- The Supervisor controls the host's Docker daemon. Anyone who can reach its token can create containers with host authority. Keep the token in `.env`, keep the Supervisor on the internal network, and do not publish port 8787.
- Workers share the host kernel. Use a dedicated host; a stronger per-Computer boundary such as a VM or gVisor is not part of this release.
- The public origin is Caddy's; do not publish the Space's port 7337.
- Bots on one Computer share its Workspace and are not isolated from each other.

## What this release does not do

- Deleting a Hosted Computer, or removing the last Bot from one, does not remove its worker or Workspace root yet.
- There is no client control to start, stop, or wake a Hosted Computer; the lifecycle manager does that from work.
- A worker has no screen, browser, Vault credential, or Take control.
- Worker egress has no per-destination firewall.
- Kubernetes, multi-host scheduling, and remote Local Computers are out of scope.
