"""Check GUI harness behavior without a native clipboard or window server."""

import runpy
import signal
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

TEST_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(TEST_DIR))
HARNESS = runpy.run_path(str(TEST_DIR / "paste-image-gui.py"))


class NativePreviewTest(unittest.TestCase):
    """The pixels only describe a freshly captured window."""

    def test_retries_capture_using_current_window(self) -> None:
        """An unavailable window can be replaced before the next probe."""
        with tempfile.TemporaryDirectory() as scratch:
            capture = Path(scratch) / "window.png"
            capture.write_bytes(b"stale screenshot")
            window_ids = iter((b"14\n", b"15\n"))
            captures = []

            def run(args, **_kwargs):
                if args[1] == "window":
                    return subprocess.CompletedProcess(args, 0, next(window_ids), b"")
                if args[0] == "/usr/sbin/screencapture":
                    self.assertFalse(capture.exists())
                    captures.append(args[3])
                    if len(captures) == 1:
                        capture.write_bytes(b"incomplete screenshot")
                        return subprocess.CompletedProcess(args, 1, b"", b"window not ready")
                    capture.write_bytes(b"fresh screenshot")
                    return subprocess.CompletedProcess(args, 0, b"", b"")
                self.assertEqual(capture.read_bytes(), b"fresh screenshot")
                return subprocess.CompletedProcess(args, 0, b"", b"")

            with patch.object(subprocess, "run", run):
                HARNESS["wait_for_preview"](
                    "a pre-paste screenshot",
                    lambda: HARNESS["mac_window_preview"]("helper", 1, capture, {}),
                    before_paste=True,
                )
            self.assertEqual(captures, ["-l14", "-l15"])

    def test_preserves_capture_failure_at_deadline(self) -> None:
        """A missing screenshot never counts as a successful empty frame."""
        unavailable = HARNESS["WindowUnavailable"]

        def probe() -> bool:
            raise unavailable("window not ready")

        with (
            patch("time.monotonic", side_effect=(0, 1, 11)),
            patch("time.sleep"),
            self.assertRaisesRegex(AssertionError, "window not ready"),
        ):
            HARNESS["wait_for_preview"]("a pre-paste screenshot", probe, before_paste=True)

    def test_rejects_preexisting_colors(self) -> None:
        """The pre-paste check detects colored pixels immediately."""
        with self.assertRaisesRegex(AssertionError, "fixture colors before paste"):
            HARNESS["wait_for_preview"]("a pre-paste screenshot", lambda: True, before_paste=True)

    def test_waits_for_post_paste_pixels(self) -> None:
        """A successful capture can precede actual GUI rendering."""
        frames = iter((False, True))
        with patch("time.sleep"):
            HARNESS["wait_for_preview"]("the rendered preview", lambda: next(frames))


class ProcessCleanupTest(unittest.TestCase):
    """The spawned GUI group still owns descendants after its leader exits."""

    def test_escalates_for_descendant_after_leader_exits(self) -> None:
        """Waiting for the leader alone must not end cleanup."""
        child = Mock(pid=42)
        child.poll.return_value = 0
        signals = []

        def kill_group(group: int, sig: int) -> None:
            self.assertEqual(group, child.pid)
            signals.append(sig)

        with (
            tempfile.TemporaryDirectory() as scratch,
            patch.object(subprocess, "Popen", return_value=child),
            patch("os.killpg", side_effect=kill_group),
            patch("time.monotonic", side_effect=(0, 6)),
            HARNESS["process"](["fake WezTerm"], {}, Path(scratch) / "log"),
        ):
            pass
        self.assertIn(signal.SIGKILL, signals)
        child.wait.assert_called_once_with(timeout=5)


if __name__ == "__main__":
    unittest.main()
