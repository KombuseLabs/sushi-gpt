"""Candidate portability checks must reject accidental host dependencies."""

from pathlib import Path
import unittest
from unittest.mock import patch

import smoke_cli_candidate as smoke


class NativeBinaryTests(unittest.TestCase):
    def test_accepts_arm64_with_only_system_libraries(self):
        with patch.object(
            smoke,
            "run",
            side_effect=[
                "arm64",
                "codex:\n\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0)\n"
                "\t/System/Library/Frameworks/Security.framework/Security (compatibility version 1.0.0)",
            ],
        ):
            smoke.check_native_binary(Path("codex"))

    def test_rejects_host_and_unresolved_dynamic_libraries(self):
        for dependency in [
            "/opt/homebrew/opt/xz/lib/liblzma.5.dylib",
            "@rpath/libexample.dylib",
        ]:
            with (
                self.subTest(dependency=dependency),
                patch.object(
                    smoke,
                    "run",
                    side_effect=[
                        "arm64",
                        f"codex:\n\t{dependency} (compatibility version 1.0.0)",
                    ],
                ),
                self.assertRaisesRegex(RuntimeError, "Non-system dynamic dependency"),
            ):
                smoke.check_native_binary(Path("codex"))

    def test_rejects_wrong_architecture(self):
        with (
            patch.object(smoke, "run", return_value="x86_64"),
            self.assertRaisesRegex(RuntimeError, "Expected an arm64"),
        ):
            smoke.check_native_binary(Path("codex"))


if __name__ == "__main__":
    unittest.main()
