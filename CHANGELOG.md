# SushiGPT changelog

SushiGPT is currently published as source code on `main`. No versioned SushiGPT
release or downloadable binary has been published yet. See the
[installation guide](docs/install.md) to build this fork.

## 2026-09-24 — Initial public source publication

[PR #1](https://github.com/KombuseLabs/sushi-gpt/pull/1) was merged as
[`c3f60dc92b`](https://github.com/KombuseLabs/sushi-gpt/commit/c3f60dc92b88f3309e4213c6714efdec8178fcaf).

- Optional model and provider routing at native agent spawning, using fixed rules
  or the Jev classifier while preserving the native agent lifecycle.
- A Claude Code CLI transport with native tool execution, an OpenResponses
  adapter, typed execution errors and opt-in local diagnostics.
- Four Sushi runtime crates behind native extension contracts, routing examples
  and fork-specific contribution guidance.
- A fix for parameterless Claude tool calls followed by an empty argument delta.

The PR records local component and native routing tests and a live Claude check
on an earlier extraction commit. Those checks do not establish validation of the
exact merge commit, the full workspace, cross-platform behavior, or desktop and
voice integration. See the [README status](README.md#status) for current limits.

Follow [SushiGPT commits](https://github.com/KombuseLabs/sushi-gpt/commits/main/)
for subsequent source changes and [SushiGPT releases](https://github.com/KombuseLabs/sushi-gpt/releases)
for future versioned releases. [OpenAI Codex releases](https://github.com/openai/codex/releases)
describe upstream Codex; they do not contain SushiGPT's additions.
