# SushiGPT: native subagent model routing

This fork adds optional model-default rules at Codex's existing agent-spawn
configuration boundary. Native agent identity, communication, status, limits,
interruption, and result handling remain responsible for the child lifecycle.
No companion UI or separate worker scheduler is required.

Routing is off unless explicitly enabled. Rules inspect the existing agent
role and a task field exposed by the spawn API: V1's plaintext message or V2's
plaintext task name. They never inspect encrypted descriptions, parent history,
or document contents.
The fixed rules are deterministic. The optional Jev classifier below can handle
unmatched V2 task names. Add rules
to the existing Codex `config.toml`:

```toml
[agent_model_routing]
enabled = true

[[agent_model_routing.rules]]
agent_type = "reviewer"
model = "your-available-review-model"
reasoning_effort = "high"

[[agent_model_routing.rules]]
# V2: matches task_name, e.g. "summarize_report".
task_name_contains = ["summarize", "extract"]
model = "your-available-analysis-model"
```

Replace the example identifiers with models in the configured catalog. Omitting
`model_provider` keeps the current provider; cross-provider requirements are below. Roles must also be configured normally. The first matching rule wins.
Role matching is exact; task matching is case-insensitive and matches any listed
substring. A supplied role filter must also match. Each rule needs at least one
non-empty matcher. At most 32 rules and 16 task matchers per rule are accepted,
with a 256-byte limit per value.

- `task_name_contains` applies only to V2 `spawn_agent.task_name`, a local task
  name such as `summarize_report`, not the canonical `/root/...` path. It works
  regardless of whether the task message uses encrypted or plaintext transport.
- `task_contains` applies only to the V1 task message/input preview. A rule with
  this field never matches a V2 spawn, even if its message happens to be plaintext.
- The two task filters cannot appear together in one rule. Use separate ordered
  rules for V1 and V2. A role-only rule applies to either API.

For V2 rules, put the intended marker in `task_name` and use
`task_name_contains`. A marker only in the task message does not match.

Rules apply to fresh or partial-history children when the spawn call omits an
explicit model. A full-history fork retains existing behavior. In V2, request a
fresh child with `fork_turns = "none"` if independent model routing is desired.
An explicit reasoning effort remains explicit. The existing role-configuration
precedence still applies after these defaults, including role-level model settings.
With routing disabled, existing model defaults and inheritance are unchanged.
When enabled rules miss, Jev may supply defaults as described below. Without a
Jev selection, native defaults apply unless `strict_candidates` rejects the spawn.

Routed models use the same catalog, reasoning-effort, service-tier, and runtime
permission validation as ordinary native spawns. An unavailable configured model
fails through the existing spawn error path; it is not silently replaced.

The implementation supports routing within the current provider and V2 children on
configured `openresponses` or `claude_cli` providers. It does not discover quotas,
infer costs, choose models from measured quality, or provide a separate voice/UI
integration. No particular target model is required.

## Plaintext messages and cross-provider children

A provider-encrypted task assignment cannot be read by a child on another
provider. Set `agent_model_routing.plaintext_messages = true` and choose a
`features.multi_agent_v2.tool_namespace` other than the reserved `collaboration`
namespace. Configuration rejects plaintext mode with the reserved namespace,
whose tool definitions are owned by the backend.

Plaintext mode changes the `message` schema of `spawn_agent`, `send_message` and
`followup_task`. Under the custom namespace, an empty or omitted
`encrypted_function_args` list is treated as plaintext; a nonempty list remains
encrypted. Existing encrypted assignments are never decrypted or reinterpreted.
This setting exposes task messages to the selected child provider; it does not
expand the metadata sent to Jev.

Set `model_provider` on a rule or Jev class to select another configured provider.
Cross-provider spawning requires V2, `fork_turns = "none"`, a plaintext assignment,
and an authoritative `model_catalog_json` containing the parent and candidate
models. Native required-provider constraints remain enforced. A role override
that changes the routed model is rejected for a cross-provider child.

- `wire_api = "openresponses"` requires a nonempty credential in the provider's
  configured `env_key` before child startup. Its adapter supports a restricted
  standard Responses contract; compatible catalog metadata is required. The
  [hosted catalog example](hosted-model.example.json) is illustrative, not a
  compatibility certification for a service or model.
- `wire_api = "claude_cli"` requires an explicit `cli_command` executable. It uses
  CLI-managed authentication and rejects HTTP authentication/provider settings,
  `requires_openai_auth = true`, and WebSockets. The native child owns the CLI
  process; its tools execute through Codex's ToolRouter. A local CLI transport
  does not imply local inference: the CLI can transmit task content to its service.
- Both foreign transports require fresh children, even when already selected as
  the current provider; partial and full history forks are unsupported.

`inherit_dynamic_tools = true` separately opts fresh children into host-defined
dynamic tools. Enabling routing alone does not enable that inheritance. Review
the tools and data made available to the selected child provider.

### Strict candidate selection

`strict_candidates` defaults to `false`. When enabled, routing must be enabled
and Jev candidate classes must be nonempty. Each class needs an explicit
`model_provider` and `text`, `tools`, and `streaming` capabilities; its
`dynamic_tools` capability must agree with `inherit_dynamic_tools`. These are
configuration declarations, not runtime capability discovery.

The final provider, model and reasoning effort must exactly match a configured
Jev class, including after explicit choices or role overrides. A failed or skipped
classification that would otherwise use native defaults instead fails the spawn;
no replacement child starts. Fixed rules can still select an allowed candidate.
Explicit model choices and full-history forks skip routing but remain subject to
the candidate check and transport restrictions.

## Optional Jev classification

Jev is opt-in at **both** `agent_model_routing.enabled` and
`agent_model_routing.jev.enabled`. Start with [jev-routing.example.toml](jev-routing.example.toml).
Its candidate mappings are illustrative and must be calibrated for your workload,
available models and billing; this implementation does not infer prices from model
names, discover remaining quotas, measure savings, or evaluate completed work.

Native precedence remains: explicit model/full-history behavior, then the first
matching fixed rule. Only a fresh or partial-history V2 child without either
choice may call Jev. V1 continues to use fixed rules and never sends its plaintext
message to Jev. Explicit effort remains explicit; existing role overrides still
apply afterward. A role may therefore replace a same-provider Jev-selected model,
subject to strict candidate validation. Cross-provider role changes are restricted as above. The
final routing log reports the model after role overrides.

**Data sent:** only the local V2 `task_name`, effective role name (`default` when
omitted), configured classifier instructions and class descriptions/labels. There
is no plaintext description, ciphertext, parent history, file content, repository
path, or target-model name in the classifier payload. Task names and roles can
still disclose information: enabling this option authorizes their transmission to
TypeSafe. Names often omit important context; Jev cannot inspect the encrypted
assignment or reliably determine its true difficulty. Enabling Jev itself does
not change the spawn-tool schema or encryption behavior; `plaintext_messages`
is a separate setting.

The client implements the official [TypeSafe Choice API](https://docs.typesafe.ai/api)
at `POST https://api.typesafe.ai/v1/systemone` with bearer authentication. The
[versioned model](https://docs.typesafe.ai/models) defaults to `jev-1.13.0` and is
configurable. Only that official endpoint and literal HTTP loopback addresses
(`/v1/systemone`) are accepted; loopback is for synthetic mock tests. Redirects
are rejected. The proxy policy of the native configuration remains in force.

Each class maps a description to a native model and optional effort. All candidate
models must exist in the native cached catalog and support the native agent
version. Cross-provider candidates also require the provider configuration and
shared catalog described above. The selected effort goes through native
validation. `abstain` is an automatic extra class for insufficient evidence; user classes cannot reuse it.
A valid response must contain a complete finite probability distribution, sum to
one within 0.001, and select its unique largest entry. With
`strict_candidates = false`, low confidence (default gate 0.8), abstention,
ties, invalid distributions and failed candidate preflight preserve native
defaults. Invalid target settings for a class without an explicit provider also
fall back. Once a class with `model_provider` is selected, provider application
and native model/role validation can fail the spawn instead of falling back.
The confidence gate uses the API answer's separate `confidence` field, which must
be finite and within [0, 1]. The probability distribution is validated separately;
the implementation does not require `confidence` to equal the selected class's
probability. Neither value establishes the chance that the child will complete
the task correctly; calibrate the threshold for the workload.

Missing/empty classifier credentials, HTTP failures (including 401/429/529),
malformed or oversized responses and network failures also fall back to native
defaults when `strict_candidates = false`. In strict mode these fallback paths
fail spawning. Each eligible spawn makes at most one request, no retry, under a total HTTP timeout
(default 1500 ms, configurable 100–10000 ms) and 32 KiB response cap. Native
configuration/catalog/role errors unrelated to Jev retain their existing behavior.
The classifier uses direct HTTP, so its call cannot re-enter agent routing.

Credentials come only from `TYPESAFE_API_KEY` (or the configured `api_key_env`) in
the native process environment. Do not put a key in this file or a chat. In zsh,
enter it locally without echo or a shell-history literal, then launch the native
CLI from that same terminal:

```sh
read -rs 'TYPESAFE_API_KEY?TypeSafe API key: '; printf '\n'
export TYPESAFE_API_KEY
# Launch your configured native CLI or test script here.
unset TYPESAFE_API_KEY
```

A GUI or already-running shared daemon does not acquire a newly exported variable;
start a new native process (`--no-daemon` for the TUI). Use a separate `CODEX_HOME`
for isolated configuration and credentials.

Use `RUST_LOG=agent_model_routing=info` for decisions: class/confidence and configured
classifier version, typed fallback reason, and final model/effort after role
settings. No key, HTTP body, task name or full task content is added to these logs.
Existing session logs retain their normal behavior. A successful answer alone is
not proof of model selection: inspect the child request model and the routing log.

Tests cover the HTTP contract with synthetic data and a native-spawn matrix that
preserves encrypted messages and checks actual outbound child model/effort,
including rule/explicit/role precedence, disabled/unconfigured behavior, partial
and full forks, V1, missing key, unavailable model, low confidence, outage and timeout.
A live TypeSafe call requires a locally supplied key and remains a separate check.

## Native integration and runtime switch

Routing is statically installed through the native `ExtensionRegistry`. The
app-server and thread-manager sample register the same implementations:

- `codex-rs/sushi/routing-policy` owns config DTOs, validation and rule matching.
  `codex-config` re-exports the existing configuration path.
- `codex-rs/sushi/routing` owns routing decisions, runtime control and the bounded
  classifier request. A read-only host capability validates native candidates.
- `codex-rs/sushi/claude-transport` owns the local process and wire protocol behind
  a duplex sampling contract. Core alone executes tools and controls child/turn
  lifetimes; process cleanup observes native cancellation and stream closure.
- `codex-rs/sushi/diagnostics` owns the opt-in bounded JSONL writer and observers.
  OpenResponses adaptation remains in `codex-api`.

The existing child-configuration boundary applies proposals before native
validation and role settings, then checks strict candidates after resolution.
Core projects prompt and metadata snapshots without exposing private sessions.
Configured routing or local transport without its registered implementation
fails explicitly; hosts with no routing configuration retain native defaults.
Direct `ThreadManager` embedders can register the three runtime crates with
`install(&mut registry)`. No dynamic loader or separate worker scheduler is used.

Routing integration tests start at `codex-rs/core/tests/agent_model_routing.rs`,
with hosted, runtime and telemetry modules alongside it. Cargo discovers the
integration target; `codex-rs/core/BUILD.bazel` includes the test and fixture data.

### During a running session

After starting a binary containing this implementation, use the helper from a
separate terminal. Supply the **same CODEX_HOME directory** used to start that
native session. The explicit argument prevents accidentally changing another home:

```sh
python3 scripts/agent-model-routing.py off --codex-home /absolute/path/to/session-home
python3 scripts/agent-model-routing.py rules-only --codex-home /absolute/path/to/session-home
python3 scripts/agent-model-routing.py configured --codex-home /absolute/path/to/session-home
python3 scripts/agent-model-routing.py status --codex-home /absolute/path/to/session-home
```

- `off`: disable fixed-rule and Jev selection. Native explicit choices,
  inherited/default models and effort, roles, and validation remain intact. With
  `strict_candidates = true`, a would-be default spawn is rejected; the switch
  does not disable candidate validation, plaintext mode or tool inheritance.
- `rules-only`: leave configured fixed rules available; disable Jev.
- `configured`: restore the session's original opt-in configuration. It never
  overrides `agent_model_routing.enabled = false` or enables disabled Jev.
- `status`: inspect the local control state. This is not a check that a process is
  running or that its config enables routing.

The helper atomically replaces a small `agent-model-routing.mode` file under that
home. `config.toml` is untouched. Missing control file means `configured`;
unreadable, malformed, oversized, non-regular or symlinked control files disable
routing selection with a diagnostic. Native defaults apply unless strict candidate
validation rejects the spawn. No credential is needed by the helper.

Each new routing decision reads one control snapshot. Changes affect decisions
that start after the update, including subsequent spawns in the same parent turn;
an already-started selection can finish with its previous snapshot. All sessions
and children sharing that home observe the switch. Running children are never
reconfigured, interrupted, or restarted. Thus a child that later spawns another
child applies the switch only to that new spawn, while retaining its own model.
Use separate CODEX_HOME directories for independent runtime switches. The override
persists across process restarts until changed back to `configured` or removed.

The switch changes only routing mode. The session continues to use its loaded
rules and opt-in flags; changing those requires a new session. After installing
a build that adds routing support, restart into that binary once. Subsequent
mode changes require no restart.
