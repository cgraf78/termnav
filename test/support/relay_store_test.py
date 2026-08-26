"""Unit tests for the relay's nonce-less terminal commit store."""

from __future__ import annotations

import importlib.util
import json
import os
import pathlib
import sys
import tempfile
import time
import unittest
from importlib.machinery import SourceFileLoader
from unittest import mock

ROOT = pathlib.Path(__file__).resolve().parents[2]
RELAY = ROOT / "bin" / "termnav-relay"
spec = importlib.util.spec_from_loader(
    "termnav_relay_store_test",
    SourceFileLoader("termnav_relay_store_test", str(RELAY)),
)
if spec is None or spec.loader is None:
    raise RuntimeError(f"cannot import {RELAY}")
relay = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = relay
spec.loader.exec_module(relay)
relay.load_commit_runtime()


class RelayStoreTest(unittest.TestCase):
    """Exercise ordering and fail-closed state independently from tmux."""

    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory(prefix="termnav-store-")
        self.environment = mock.patch.dict(os.environ, {"XDG_RUNTIME_DIR": self.tempdir.name})
        self.environment.start()
        self.socket = f"{self.tempdir.name}/tmux.sock"

    def tearDown(self) -> None:
        self.environment.stop()
        self.tempdir.cleanup()

    def directive(
        self,
        nonce: str,
        *,
        tty: str = "/dev/pts/1",
        pid: int = 101,
        created: int = 1001,
    ) -> dict:
        """Build the minimal complete forwarding directive schema."""

        return {
            "v": 2,
            "nonce": nonce,
            "action": "forward",
            "session": "$1",
            "forward_pane": "%9",
            "tmux_socket": self.socket,
            "local_client_tty": tty,
            "local_client_pid": pid,
            "local_client_created": created,
            "started_at": int(time.time()),
            "owner_pid": os.getpid(),
        }

    def claim(
        self,
        tty: str = "/dev/pts/1",
        pid: int = 101,
        created: int = 1001,
    ) -> dict | None:
        """Claim for a prevalidated synthetic client identity."""

        return relay._claim_directive_for_identity(self.socket, tty, pid, created)

    def test_pending_directive_is_invisible_until_commit(self) -> None:
        directive = self.directive("111111111111")
        relay._arm_directive_and_seq(self.socket, directive)

        self.assertIsNone(self.claim())
        self.assertTrue(relay.mark_committed(self.socket, directive["nonce"]))
        claimed = self.claim()

        self.assertEqual(directive["nonce"], claimed["nonce"])

    def test_receipt_keeps_the_client_exclusive_until_execution_finishes(self) -> None:
        first = self.directive("222222222222")
        second = self.directive("333333333333")
        relay._arm_directive_and_seq(self.socket, first)
        relay.mark_committed(self.socket, first["nonce"])
        claimed = self.claim()

        with self.assertRaisesRegex(RuntimeError, "outstanding"):
            relay._arm_directive_and_seq(self.socket, second)

        relay.record_result(self.socket, claimed["nonce"], True)
        relay._arm_directive_and_seq(self.socket, second)
        self.assertTrue(relay.directive_path(self.socket, second["nonce"]).exists())

    def test_dead_owner_pending_preparation_does_not_block_recovery(self) -> None:
        abandoned = self.directive("232323232323")
        replacement = self.directive("242424242424")
        relay._arm_directive_and_seq(self.socket, abandoned)

        with mock.patch.object(relay.os, "kill", side_effect=ProcessLookupError):
            relay._arm_directive_and_seq(self.socket, replacement)

        self.assertFalse(relay.directive_path(self.socket, abandoned["nonce"]).exists())
        self.assertTrue(relay.directive_path(self.socket, replacement["nonce"]).exists())

    def test_stale_pending_preparation_from_live_server_is_reclaimed(self) -> None:
        abandoned = self.directive("252525252525")
        replacement = self.directive("262626262626")
        with mock.patch.object(relay.time, "time", return_value=100):
            relay._arm_directive_and_seq(self.socket, abandoned)

        with mock.patch.object(
            relay.time,
            "time",
            return_value=100 + relay.PREPARE_STALE_SECONDS + 1,
        ):
            relay._arm_directive_and_seq(self.socket, replacement)

        self.assertFalse(relay.directive_path(self.socket, abandoned["nonce"]).exists())
        self.assertTrue(relay.directive_path(self.socket, replacement["nonce"]).exists())

    def test_fast_receipt_is_observed_after_the_directive_is_removed(self) -> None:
        directive = self.directive("343434343434")
        relay._arm_directive_and_seq(self.socket, directive)
        relay.mark_committed(self.socket, directive["nonce"])
        claimed = self.claim()
        relay.record_result(self.socket, claimed["nonce"], True)

        self.assertTrue(relay.wait_for_consumption(self.socket, directive["nonce"]))

    def test_lost_reply_poisons_only_that_physical_client(self) -> None:
        lost = self.directive("444444444444")
        relay._arm_directive_and_seq(self.socket, lost)
        relay.mark_committed(self.socket, lost["nonce"])

        with mock.patch.object(relay, "COMMIT_TIMEOUT", 0.02):
            self.assertFalse(relay.wait_for_consumption(self.socket, lost["nonce"]))

        with self.assertRaisesRegex(RuntimeError, "poisoned"):
            relay._arm_directive_and_seq(self.socket, self.directive("555555555555"))

        # A delayed nonce-less User8 cannot claim a later action after the
        # original commit RPC became indeterminate.
        self.assertIsNone(self.claim())

        # tmux's client creation stamp distinguishes a newly attached client
        # even if the OS eventually reuses both its PID and tty pathname.
        replacement = self.directive("666666666666", created=1002)
        relay._arm_directive_and_seq(self.socket, replacement)
        relay.mark_committed(self.socket, replacement["nonce"])
        self.assertTrue(relay.directive_path(self.socket, replacement["nonce"]).exists())
        self.assertEqual(replacement["nonce"], self.claim(created=1002)["nonce"])

    def test_ready_directive_has_no_topology_depth_expiry(self) -> None:
        directive = self.directive("676767676767")
        with mock.patch.object(relay.time, "time", return_value=100):
            relay._arm_directive_and_seq(self.socket, directive)
            relay.mark_committed(self.socket, directive["nonce"])

        with mock.patch.object(relay.time, "time", return_value=100_000):
            claimed = self.claim()

        self.assertEqual(directive["nonce"], claimed["nonce"])

    def test_ready_directive_from_dead_server_cannot_claim_a_late_reply(self) -> None:
        abandoned = self.directive("686868686868")
        relay._arm_directive_and_seq(self.socket, abandoned)
        relay.mark_committed(self.socket, abandoned["nonce"])

        with mock.patch.object(relay.os, "kill", side_effect=ProcessLookupError):
            self.assertIsNone(self.claim())

        self.assertFalse(relay.directive_path(self.socket, abandoned["nonce"]).exists())
        with self.assertRaisesRegex(RuntimeError, "poisoned"):
            relay._arm_directive_and_seq(
                self.socket,
                self.directive("696969696969"),
            )

    def test_recreated_client_skips_older_ready_identity_with_same_pid_tty(
        self,
    ) -> None:
        old = self.directive("707070707070", created=1001)
        current = self.directive("717171717171", created=1002)
        relay._arm_directive_and_seq(self.socket, old)
        relay.mark_committed(self.socket, old["nonce"])
        relay._arm_directive_and_seq(self.socket, current)
        relay.mark_committed(self.socket, current["nonce"])

        claimed = self.claim(created=1002)

        self.assertEqual(current["nonce"], claimed["nonce"])
        self.assertTrue(relay.directive_path(self.socket, old["nonce"]).exists())

    def test_sequences_remain_monotonic_across_distinct_clients(self) -> None:
        later_name = self.directive("ffffffffffff")
        earlier_name = self.directive("aaaaaaaaaaaa", tty="/dev/pts/2", pid=202)
        relay._arm_directive_and_seq(self.socket, later_name)
        relay._arm_directive_and_seq(self.socket, earlier_name)
        relay.mark_committed(self.socket, later_name["nonce"])
        relay.mark_committed(self.socket, earlier_name["nonce"])

        first = self.claim()
        second = self.claim("/dev/pts/2", 202)

        self.assertLess(first["seq"], second["seq"])
        self.assertEqual("ffffffffffff", first["nonce"])
        self.assertEqual("aaaaaaaaaaaa", second["nonce"])

    def test_malformed_publication_is_removed_without_execution(self) -> None:
        path = relay.directive_path(self.socket, "777777777777")
        path.write_text('{"v":2', encoding="utf-8")

        self.assertIsNone(self.claim())
        self.assertFalse(path.exists())

    def test_files_are_owner_only_even_under_a_hostile_umask(self) -> None:
        path = relay.directive_dir(self.socket) / "permissions.json"
        old_umask = os.umask(0o777)
        try:
            relay._publish_exclusive(path, json.dumps({}))
        finally:
            os.umask(old_umask)

        self.assertEqual(0o600, path.stat().st_mode & 0o777)


if __name__ == "__main__":
    unittest.main(verbosity=2)
