"""Only temporary control homes; never reads credentials or a real user configuration."""

import importlib.util
import io
from pathlib import Path
import tempfile
import unittest
from contextlib import redirect_stdout
from unittest.mock import patch

script = Path(__file__).with_name("agent-model-routing.py")
spec = importlib.util.spec_from_file_location("routing_control", script)
control = importlib.util.module_from_spec(spec)
spec.loader.exec_module(control)


class ControlTests(unittest.TestCase):
    def call(self, home, mode):
        output = io.StringIO()
        with (
            patch("sys.argv", [str(script), mode, "--codex-home", str(home)]),
            redirect_stdout(output),
        ):
            control.main()
        return output.getvalue()

    def test_updates_and_reports_without_modifying_config(self):
        with tempfile.TemporaryDirectory() as temp:
            home = Path(temp)
            config = home / "config.toml"
            config.write_text("[agent_model_routing]\nenabled = false\n")
            original = config.read_bytes()
            self.assertIn("configured (no override)", self.call(home, "status"))
            for mode in ("off", "rules-only", "configured"):
                self.call(home, mode)
                self.assertEqual(
                    (home / control.FILENAME).read_bytes(),
                    (mode + "\n").encode("ascii"),
                )
                self.assertIn("Runtime override: " + mode, self.call(home, "status"))
                self.assertEqual(config.read_bytes(), original)
                self.assertEqual(list(home.glob(".routing-mode-*")), [])
            (home / control.FILENAME).write_bytes(b"invalid")
            self.assertIn("off (invalid control file)", self.call(home, "status"))

    def test_failed_atomic_replace_preserves_previous_mode(self):
        with tempfile.TemporaryDirectory() as temp:
            home = Path(temp)
            self.call(home, "off")
            with patch.object(
                control.os, "replace", side_effect=PermissionError("fixture")
            ):
                with self.assertRaises(PermissionError):
                    self.call(home, "configured")
            self.assertEqual((home / control.FILENAME).read_text(), "off\n")
            self.assertEqual(list(home.glob(".routing-mode-*")), [])


if __name__ == "__main__":
    unittest.main()
