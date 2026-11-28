"""Unit tests for the worker Supervisor's request construction."""

import os
import shutil
import tempfile
import unittest
from pathlib import Path

os.environ.setdefault("NEHA_SUPERVISOR_TOKEN", "test")
import supervisor  # noqa: E402


class WorkerRequestTests(unittest.TestCase):
    def setUp(self) -> None:
        self.data = tempfile.TemporaryDirectory()
        self.workspaces = tempfile.TemporaryDirectory()
        self.previous = supervisor.HOST_DATA_DIR
        self.previous_workspaces = supervisor.CONTAINER_WORKSPACES_DIR
        supervisor.HOST_DATA_DIR = Path(self.data.name).resolve()
        supervisor.CONTAINER_WORKSPACES_DIR = Path(self.workspaces.name).resolve()
        (supervisor.HOST_DATA_DIR / "workspaces" / "01a22192-5fb4-705f-b0c8-9cf53048763a").mkdir(parents=True)
        (supervisor.CONTAINER_WORKSPACES_DIR / "01a22192-5fb4-705f-b0c8-9cf53048763a").mkdir()

    def tearDown(self) -> None:
        supervisor.HOST_DATA_DIR = self.previous
        supervisor.CONTAINER_WORKSPACES_DIR = self.previous_workspaces
        self.data.cleanup()
        self.workspaces.cleanup()

    def request(self) -> dict:
        return supervisor.container_config(
            "01a22192-5fb4-705f-b0c8-9cf53048763a",
            "workspaces/01a22192-5fb4-705f-b0c8-9cf53048763a",
            "worker-token",
        )

    def test_a_worker_has_no_public_port_and_only_its_workspace(self) -> None:
        request = self.request()
        host = request["HostConfig"]
        self.assertEqual(
            host["Binds"],
            [
                str(
                    supervisor.HOST_DATA_DIR
                    / "workspaces"
                    / "01a22192-5fb4-705f-b0c8-9cf53048763a"
                )
                + ":/workspace:rw"
            ],
        )
        self.assertTrue(host["ReadonlyRootfs"])
        self.assertEqual(host["CapDrop"], ["ALL"])
        self.assertTrue(host["Init"])
        self.assertIn("no-new-privileges", host["SecurityOpt"])
        self.assertEqual(host["Tmpfs"]["/tmp"].split(",")[:2], ["rw", "noexec"])
        self.assertNotIn("ExposedPorts", request)
        self.assertNotIn("PortBindings", host)

    def test_workspace_cannot_leave_the_data_directory(self) -> None:
        for workspace in (
            "workspaces/../../etc",
            "/etc",
            "other/01a22192-5fb4-705f-b0c8-9cf53048763a",
        ):
            with self.subTest(workspace=workspace):
                with self.assertRaises(ValueError):
                    supervisor.workspace_host_path(workspace)

    def test_a_workspace_symlink_outside_the_mount_is_refused(self) -> None:
        computer = "01a22192-5fb4-705f-b0c8-9cf53048763a"
        outside = tempfile.TemporaryDirectory()
        try:
            link = supervisor.CONTAINER_WORKSPACES_DIR / computer
            if link.is_symlink():
                link.unlink()
            else:
                shutil.rmtree(link)
            link.symlink_to(Path(outside.name).resolve(), target_is_directory=True)
            with self.assertRaises(ValueError):
                supervisor.container_config(computer, f"workspaces/{computer}", "token")
        finally:
            outside.cleanup()
            link = supervisor.CONTAINER_WORKSPACES_DIR / computer
            if link.is_symlink():
                link.unlink()
            link.mkdir()

    def test_workspace_must_belong_to_the_computer(self) -> None:
        another = "01a22193-0000-7000-8000-000000000000"
        (supervisor.CONTAINER_WORKSPACES_DIR / another).mkdir()
        with self.assertRaises(ValueError):
            supervisor.container_config(
                "01a22192-5fb4-705f-b0c8-9cf53048763a",
                f"workspaces/{another}",
                "token",
            )

    def test_a_workspace_symlink_with_the_same_name_is_refused(self) -> None:
        computer = "01a22192-5fb4-705f-b0c8-9cf53048763a"
        other = "01a22194-0000-7000-8000-000000000000"
        nested = supervisor.CONTAINER_WORKSPACES_DIR / other / "bot" / computer
        nested.mkdir(parents=True)
        link = supervisor.CONTAINER_WORKSPACES_DIR / computer
        if link.is_symlink():
            link.unlink()
        else:
            shutil.rmtree(link)
        link.symlink_to(nested, target_is_directory=True)
        with self.assertRaises(ValueError):
            supervisor.container_config(computer, f"workspaces/{computer}", "token")

    def test_egress_selects_a_network(self) -> None:
        previous = supervisor.WORKER_EGRESS
        try:
            supervisor.WORKER_EGRESS = "restricted"
            self.assertEqual(
                self.request()["HostConfig"]["NetworkMode"],
                supervisor.INTERNAL_NETWORK,
            )
            supervisor.WORKER_EGRESS = "auto"
            self.assertEqual(
                self.request()["HostConfig"]["NetworkMode"],
                supervisor.PUBLIC_NETWORK,
            )
            supervisor.WORKER_EGRESS = "open"
            self.assertEqual(
                self.request()["HostConfig"]["NetworkMode"],
                supervisor.PUBLIC_NETWORK,
            )
            supervisor.WORKER_EGRESS = "sometimes"
            with self.assertRaises(ValueError):
                self.request()
        finally:
            supervisor.WORKER_EGRESS = previous

    def test_memory_units(self) -> None:
        self.assertEqual(supervisor.memory_bytes("512m"), 512 * 1024**2)
        self.assertEqual(supervisor.memory_bytes("1g"), 1024**3)
        self.assertEqual(supervisor.memory_bytes("1024"), 1024)


if __name__ == "__main__":
    unittest.main()
