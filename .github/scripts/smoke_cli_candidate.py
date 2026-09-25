"""Check an extracted unsigned macOS ARM64 CLI candidate without model calls."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT / "scripts"))
os.environ.setdefault("CODEX_REPO_ROOT", str(REPO_ROOT))

from codex_package.layout import validate_package_dir
from codex_package.targets import PACKAGE_VARIANTS, TARGET_SPECS

TARGET = "aarch64-apple-darwin"


def run(command, *, cwd=None, env=None):
    result = subprocess.run(
        command, cwd=cwd, env=env, text=True, capture_output=True, timeout=45
    )
    if result.returncode:
        raise RuntimeError(f"{command[0]} failed: {result.stdout}\n{result.stderr}")
    return result.stdout.strip()


def check_native_binary(path):
    if run(["lipo", "-archs", str(path)]) != "arm64":
        raise RuntimeError(f"Expected an arm64 executable: {path.name}")
    dependencies = run(["otool", "-L", str(path)]).splitlines()[1:]
    for line in dependencies:
        library = line.strip().split(" (", 1)[0]
        if not library.startswith(("/usr/lib/", "/System/Library/")):
            raise RuntimeError(
                f"Non-system dynamic dependency in {path.name}: {library}"
            )


def smoke(archive_path, report_path):
    with tempfile.TemporaryDirectory(prefix="sushi-cli-smoke-") as temporary:
        work = Path(temporary)
        package = work / "package"
        package.mkdir()
        with tarfile.open(archive_path, "r:gz") as archive:
            archive.extractall(package, filter="data")
        validate_package_dir(
            package, PACKAGE_VARIANTS["codex"], TARGET_SPECS[TARGET], include_zsh=True
        )
        metadata = json.loads((package / "codex-package.json").read_text())
        executables = [
            "bin/codex",
            "bin/codex-code-mode-host",
            "codex-path/rg",
            "codex-resources/zsh/bin/zsh",
        ]
        for relative in executables:
            check_native_binary(package / relative)

        config = work / "config"
        config.mkdir()
        (config / "config.toml").write_text("[analytics]\nenabled = false\n")
        env = dict(os.environ)
        env.update(CODEX_HOME=str(config), ZDOTDIR=str(work), PATH="/usr/bin:/bin")
        env.pop("BASH_ENV", None)
        checks = [
            ("bin/codex", ["--version"], f"codex-cli {metadata['version']}"),
            ("bin/codex", ["--help"], "Usage:"),
            ("bin/codex", ["features", "list"], "code_mode"),
            ("bin/codex", ["completion", "bash"], "codex"),
            ("bin/codex-code-mode-host", ["--help"], "Usage:"),
            ("codex-path/rg", ["--version"], "ripgrep"),
            (
                "codex-resources/zsh/bin/zsh",
                ["-f", "-c", "printf candidate-smoke"],
                "candidate-smoke",
            ),
        ]
        for executable, arguments, expected in checks:
            output = run([str(package / executable), *arguments], cwd=work, env=env)
            if expected not in output:
                raise RuntimeError(
                    f"Unexpected output from {executable} {arguments}: {output}"
                )
        report = {
            "archive": archive_path.name,
            "target": TARGET,
            "packageVersion": metadata["version"],
            "checksPassed": len(checks),
            "systemLibrariesOnly": True,
            "liveProviderCalls": False,
            "executables": {
                relative: hashlib.sha256((package / relative).read_bytes()).hexdigest()
                for relative in executables
            },
        }
        report_path.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    smoke(args.archive, args.report)
