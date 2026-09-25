## Installing & building SushiGPT

SushiGPT is currently distributed as source code. There are no SushiGPT version
tags, binary releases, or release DotSlash manifests yet. Packages and downloads
published by OpenAI install upstream Codex, not this fork. The executable built
from this repository is still named `codex`.

### System requirements

| Requirement       | Details                                                         |
| ----------------- | --------------------------------------------------------------- |
| Operating systems | macOS 12+, Ubuntu 20.04+/Debian 10+, or Windows 11 **via WSL2** |
| Git               | 2.23+ for cloning the source and built-in PR helpers            |
| RAM               | 4-GB minimum (8-GB recommended)                                 |

These are inherited Codex runtime requirements, not a verified SushiGPT platform
matrix or a Rust build memory budget. Cross-platform SushiGPT acceptance remains
unverified. Source builds also need Git, Python 3, a native C/C++ toolchain and
substantial free disk space. Rustup selects the Rust version pinned in
`codex-rs/rust-toolchain.toml` when commands run from that directory.

### Build from source

```bash
# Clone the repository and navigate to the root of the Cargo workspace.
git clone https://github.com/KombuseLabs/sushi-gpt.git
cd sushi-gpt/codex-rs
# Record the exact source revision; main can change between builds.
git rev-parse HEAD

# Install the Rust toolchain, if necessary.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
rustup show active-toolchain

# Build the CLI and matching code-mode host together.
# dev-small reduces debug artifact size; it is not a release package.
cargo build --locked --profile dev-small \
  -p codex-cli --bin codex \
  -p codex-code-mode-host --bin codex-code-mode-host

# Check the freshly built CLI without starting a model session.
./target/dev-small/codex --version

# Launch this build directly rather than an existing codex on PATH.
./target/dev-small/codex --no-daemon
```

Keep `codex` and `codex-code-mode-host` together in the build output directory.
If you set `CARGO_TARGET_DIR`, use that directory instead of `./target` in the
commands above. The initial build downloads dependencies and prebuilt V8 inputs;
`--locked` keeps Cargo dependency resolution aligned with the checked-in lockfile.
This procedure builds a local CLI, not a signed installer or desktop application.
The current development build reports `codex-cli 0.0.0`; use the recorded Git
commit to identify the source revision rather than treating that output as a
SushiGPT release version.

The CLI uses the existing Codex configuration and authentication locations by
default. To keep this fork separate from an existing installation, set
`CODEX_HOME` to a dedicated directory before launching and configure authentication
there. Model use requires the selected provider's authentication and may incur
charges. Routing is disabled by default; see [model routing](../AGENT_MODEL_ROUTING.md)
for opt-in rules, provider requirements and the optional Jev classifier. A local
Claude transport also requires an installed and authenticated Claude Code CLI.
Desktop and voice integration are not established by a successful source build.

### Development checks

Run these from `codex-rs` in the cloned repository:

```bash
# Install helper tools used by the workspace justfile.
cargo install --locked just
cargo install --locked dotslash
cargo install --locked cargo-nextest
# DotSlash fetches pinned development tools such as buildifier on first use.

# After making changes, use the root justfile helpers (they default to codex-rs):
just fmt
just fix -p codex-sushi-routing

# Run the relevant tests (project-specific is fastest), for example:
just test -p codex-tui
# Native routing integration tests use synthetic providers:
just test -p codex-core --test agent_model_routing
# `just test` without package arguments runs the full workspace via nextest.
# Avoid `--all-features` for routine local runs because it increases build
# time and `target/` disk usage by compiling additional feature combinations.
```

## Tracing / verbose logging

SushiGPT retains Codex's `RUST_LOG` configuration for Rust logging.

The TUI records diagnostics in bounded local stores by default. Set `log_dir` explicitly to enable a plaintext TUI log for a run:

```bash
./target/dev-small/codex --no-daemon -c log_dir=./.codex-log
tail -F ./.codex-log/codex-tui.log
```

The non-interactive mode (`codex exec`) defaults to `RUST_LOG=error`, but messages are printed inline, so there is no need to monitor a separate file.

See the Rust documentation on [`RUST_LOG`](https://docs.rs/env_logger/latest/env_logger/#enabling-logging) for more information on the configuration options.
