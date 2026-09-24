#!/usr/bin/env python3
"""Restrict optional model routing for future spawns in a running native session.

Uses a small control file in the session's CODEX_HOME; never changes config.toml.
All sessions using that home observe it. 'configured' restores the config's opt-in
settings, and cannot enable a feature disabled there. Existing agents are untouched.
"""

import argparse
import os
from pathlib import Path
import tempfile

MODES = ("off", "rules-only", "configured")
FILENAME = "agent-model-routing.mode"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=(*MODES, "status"))
    parser.add_argument(
        "--codex-home",
        required=True,
        type=Path,
        help="Exact CODEX_HOME used when starting the session",
    )
    args = parser.parse_args()
    home = args.codex_home.expanduser().resolve(strict=True)
    if not home.is_dir():
        parser.error("CODEX_HOME must be an existing directory")
    target = home / FILENAME
    if args.mode == "status":
        if not os.path.lexists(target):
            mode = "configured (no override)"
        elif target.is_symlink() or not target.is_file() or target.stat().st_size > 64:
            mode = "off (invalid control file)"
        else:
            try:
                raw = target.read_bytes()
            except OSError:
                raw = b""
            valid = {s.encode(): s for m in MODES for s in (m, m + "\n")}
            mode = valid[raw].strip() if raw in valid else "off (invalid control file)"
        print(f"Runtime override: {mode}\nHome: {home}")
        print(
            "Configured enable flags still apply; this is not a process-status check."
        )
        return
    # Atomic replacement prevents a transient empty mode from enabling/disabling a spawn.
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w",
            dir=home,
            prefix=".routing-mode-",
            delete=False,
            encoding="ascii",
            newline="\n",
        ) as out:
            temporary = Path(out.name)
            out.write(args.mode + "\n")
            out.flush()
            os.fsync(out.fileno())
        os.replace(temporary, target)
        temporary = None
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)
    print(f"Runtime override: {args.mode}\nHome: {home}")
    print(
        "Applies to decisions beginning after this update; existing children are unchanged."
    )


if __name__ == "__main__":
    main()
