#!/usr/bin/env python3
"""Neha worker Supervisor.

The Space asks this process to create, start, and stop one worker container per
Hosted Computer. It owns the Docker API; the Space container never does. The
API is internal, token protected, and has no public port.
"""

from __future__ import annotations

import hmac
import http.client
import json
import os
import re
import socket
import sys
import threading
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any

DOCKER_SOCKET = "/var/run/docker.sock"
API_VERSION = "v1.43"
TOKEN = os.environ.get("NEHA_SUPERVISOR_TOKEN", "")
WORKER_IMAGE = os.environ.get("NEHA_WORKER_IMAGE", "neha-worker:dev")
INTERNAL_NETWORK = os.environ.get("NEHA_WORKER_INTERNAL_NETWORK", "neha-internal")
PUBLIC_NETWORK = os.environ.get("NEHA_WORKER_PUBLIC_NETWORK", "neha-workers")
SPACE_URL = os.environ.get("NEHA_SPACE_URL", "http://localhost")
PUBLIC_HOST = os.environ.get("NEHA_PUBLIC_HOST", "")
HOST_DATA_DIR = Path(os.environ.get("NEHA_HOST_DATA_DIR", "/data")).resolve()
CONTAINER_WORKSPACES_DIR = Path(
    os.environ.get("NEHA_CONTAINER_WORKSPACES_DIR", "/workspaces")
).resolve()
WORKER_EGRESS = os.environ.get("NEHA_WORKER_EGRESS", "auto")
WORKER_CPUS = os.environ.get("NEHA_WORKER_CPUS", "1.0")
WORKER_MEMORY = os.environ.get("NEHA_WORKER_MEMORY", "1g")
WORKER_PIDS = int(os.environ.get("NEHA_WORKER_PIDS", "256"))
BIND = os.environ.get("NEHA_SUPERVISOR_BIND", "0.0.0.0")
PORT = int(os.environ.get("NEHA_SUPERVISOR_PORT", "8787"))
UUID = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")


class DockerError(RuntimeError):
    pass


class UnixHTTPConnection(http.client.HTTPConnection):
    def __init__(self) -> None:
        super().__init__("localhost")
        self.sock_path = DOCKER_SOCKET

    def connect(self) -> None:
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(30)
        self.sock.connect(self.sock_path)


def docker(method: str, path: str, payload: dict[str, Any] | None = None) -> Any:
    body = None if payload is None else json.dumps(payload).encode()
    connection = UnixHTTPConnection()
    try:
        connection.request(
            method,
            f"/{API_VERSION}{path}",
            body=body,
            headers={"Content-Type": "application/json"} if body else {},
        )
        response = connection.getresponse()
        raw = response.read()
        if response.status >= 400:
            detail = raw.decode(errors="replace") if raw else ""
            raise DockerError(f"Docker {method} {path}: HTTP {response.status} {detail}")
        if not raw:
            return None
        try:
            return json.loads(raw)
        except json.JSONDecodeError:
            return raw.decode(errors="replace")
    finally:
        connection.close()


def memory_bytes(value: str) -> int:
    units = {"k": 1024, "m": 1024**2, "g": 1024**3}
    text = value.strip().lower()
    if text and text[-1] in units:
        return int(float(text[:-1]) * units[text[-1]])
    return int(text)


def worker_name(computer: str) -> str:
    return f"neha-worker-{computer}"


def workspace_host_path(workspace: str) -> str:
    """Returns an absolute host path inside the deployment's workspaces.

    The Supervisor container sees only the Workspace tree, read-only, at
    CONTAINER_WORKSPACES_DIR. Resolving the real path there catches a symlink
    the Space might plant; the returned host path maps the verified relative
    path onto the Docker host's bind mount.
    """
    if not isinstance(workspace, str) or not workspace:
        raise ValueError("workspace is required")
    prefix = "workspaces/"
    if not workspace.startswith(prefix):
        raise ValueError("workspace is not a Computer Workspace")
    relative = workspace[len(prefix):]
    if not UUID.match(relative):
        raise ValueError("workspace is not a Computer Workspace")

    candidate = CONTAINER_WORKSPACES_DIR / relative
    try:
        resolved = candidate.resolve(strict=True)
    except FileNotFoundError as error:
        raise ValueError("the Computer Workspace does not exist yet") from error
    if not resolved.is_relative_to(CONTAINER_WORKSPACES_DIR):
        raise ValueError("workspace leaves the data directory")
    if resolved != candidate:
        raise ValueError("workspace is not the Computer Workspace root")
    if not resolved.is_dir():
        raise ValueError("the Computer Workspace is not a directory")

    host = (HOST_DATA_DIR / "workspaces" / resolved.relative_to(CONTAINER_WORKSPACES_DIR)).resolve()
    if not host.is_relative_to(HOST_DATA_DIR):
        raise ValueError("workspace leaves the data directory")
    return str(host)


def container_config(computer: str, workspace: str, token: str) -> dict[str, Any]:
    """Builds the create request for one worker.

    A worker has no public port, a read-only root, its own tmpfs, no Linux
    capabilities, and one bind mount: the Computer's Workspace root.
    """
    host_workspace = workspace_host_path(workspace)
    if Path(host_workspace).name != computer:
        raise ValueError("workspace does not belong to the Computer")
    if WORKER_EGRESS == "restricted":
        network = INTERNAL_NETWORK
    elif WORKER_EGRESS in ("auto", "open"):
        network = PUBLIC_NETWORK
    else:
        raise ValueError("NEHA_WORKER_EGRESS must be auto, restricted, or open")
    environment = [
        f"NEHA_WORKER_SPACE_URL={SPACE_URL}",
        f"NEHA_WORKER_HOST={PUBLIC_HOST}",
        f"NEHA_WORKER_COMPUTER={computer}",
        f"NEHA_WORKER_TOKEN={token}",
        "NEHA_WORKER_WORKSPACE=/workspace",
    ]
    return {
        "Image": WORKER_IMAGE,
        "Env": environment,
        "WorkingDir": "/workspace",
        "Labels": {"neha.worker": "1", "neha.computer": computer},
        "HostConfig": {
            "Binds": [f"{host_workspace}:/workspace:rw"],
            "ReadonlyRootfs": True,
            "Tmpfs": {"/tmp": "rw,noexec,nosuid,size=256m"},
            "CapDrop": ["ALL"],
            "SecurityOpt": ["no-new-privileges"],
            "NetworkMode": network,
            "Memory": memory_bytes(WORKER_MEMORY),
            "NanoCpus": int(float(WORKER_CPUS) * 1_000_000_000),
            "PidsLimit": WORKER_PIDS,
            "Init": True,
            "RestartPolicy": {"Name": "no"},
        },
    }


def find_worker(computer: str) -> str | None:
    name = worker_name(computer)
    try:
        info = docker("GET", f"/containers/{urllib.parse.quote(name, safe='')}/json")
    except DockerError as error:
        if "HTTP 404" in str(error):
            return None
        raise
    return info.get("Id") if isinstance(info, dict) else None


def ensure_worker(computer: str, workspace: str, token: str) -> str:
    existing = find_worker(computer)
    if existing:
        return existing
    name = worker_name(computer)
    path = f"/containers/create?name={urllib.parse.quote(name, safe='')}"
    created = docker("POST", path, container_config(computer, workspace, token))
    if not isinstance(created, dict) or not created.get("Id"):
        raise DockerError("Docker did not return a container id")
    return created["Id"]


def worker_id(computer: str) -> str:
    existing = find_worker(computer)
    if not existing:
        raise DockerError("no such worker")
    return existing


def action(computer: str, name: str) -> None:
    container = worker_id(computer)
    docker("POST", f"/containers/{urllib.parse.quote(container, safe='')}/{name}")


class Handler(BaseHTTPRequestHandler):
    server_version = "neha-supervisor/0.1"

    def log_message(self, fmt: str, *args: Any) -> None:
        print(f"supervisor: {fmt % args}", file=sys.stderr)

    def authorized(self) -> bool:
        if not TOKEN:
            return False
        header = self.headers.get("Authorization", "")
        expected = f"Bearer {TOKEN}"
        return hmac.compare_digest(header, expected)

    def reply(self, status: int, payload: dict[str, Any] | None = None) -> None:
        body = b"" if payload is None else json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if body:
            self.wfile.write(body)

    def body(self) -> dict[str, Any]:
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length) if length else b""
        try:
            value = json.loads(raw or b"{}")
        except json.JSONDecodeError as error:
            raise ValueError("the request body is not JSON") from error
        if not isinstance(value, dict):
            raise ValueError("the request body is not an object")
        return value

    def route(self) -> tuple[str, list[str]]:
        path = urllib.parse.urlparse(self.path).path
        return path, [part for part in path.split("/") if part]

    def do_GET(self) -> None:
        path, _ = self.route()
        if path == "/healthz":
            self.reply(200, {"ok": True})
            return
        self.reply(404)

    def do_POST(self) -> None:
        if not self.authorized():
            self.reply(401)
            return
        try:
            path, parts = self.route()
            if parts == ["v1", "workers"]:
                request = self.body()
                computer = request.get("computer")
                workspace = request.get("workspace")
                token = request.get("token")
                if not isinstance(computer, str) or not UUID.match(computer):
                    raise ValueError("computer is not an identifier")
                if not isinstance(workspace, str):
                    raise ValueError("workspace is required")
                if not isinstance(token, str) or not token:
                    raise ValueError("token is required")
                container = ensure_worker(computer, workspace, token)
                self.reply(201, {"container": container})
                return
            if len(parts) == 4 and parts[:2] == ["v1", "workers"]:
                computer, action_name = parts[2], parts[3]
                if not UUID.match(computer):
                    raise ValueError("computer is not an identifier")
                if action_name not in ("start", "stop"):
                    self.reply(404)
                    return
                action(computer, action_name)
                self.reply(204)
                return
            self.reply(404)
        except DockerError as error:
            print(f"supervisor: {error}", file=sys.stderr)
            self.reply(502, {"error": str(error)})
        except ValueError as error:
            self.reply(400, {"error": str(error)})
        except Exception as error:  # noqa: BLE001 - the process stays available
            print(f"supervisor: unexpected {error}", file=sys.stderr)
            self.reply(500, {"error": "internal Supervisor error"})

    def do_DELETE(self) -> None:
        if not self.authorized():
            self.reply(401)
            return
        try:
            _, parts = self.route()
            if len(parts) == 3 and parts[:2] == ["v1", "workers"]:
                computer = parts[2]
                if not UUID.match(computer):
                    raise ValueError("computer is not an identifier")
                container = worker_id(computer)
                docker(
                    "DELETE",
                    f"/containers/{urllib.parse.quote(container, safe='')}?force=1",
                )
                self.reply(204)
                return
            self.reply(404)
        except DockerError as error:
            self.reply(502, {"error": str(error)})
        except ValueError as error:
            self.reply(400, {"error": str(error)})


def main() -> int:
    if not TOKEN:
        print("supervisor: NEHA_SUPERVISOR_TOKEN is required", file=sys.stderr)
        return 2
    configured = os.environ.get("NEHA_HOST_DATA_DIR", "/data")
    if not Path(configured).is_absolute():
        print("supervisor: NEHA_HOST_DATA_DIR must be absolute", file=sys.stderr)
        return 2
    HOST_DATA_DIR.mkdir(parents=True, exist_ok=True)
    server = ThreadingHTTPServer((BIND, PORT), Handler)
    print(f"supervisor: listening on {BIND}:{PORT}", file=sys.stderr)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        return 0
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
