# SushiGPT local telemetry v1

An optional local diagnostic feed for consumers of the JSONL contract below. This is observed usage, not a complete billing ledger or a change to model routing.

Start only the intended Fork process with `SUSHIGPT_TELEMETRY=1` and an explicit absolute, existing `CODEX_HOME`. The process writes `<CODEX_HOME>/sushigpt-telemetry.jsonl`; no profile is created or global configuration changed. Without opt-in, no file or writer is created. Configuration and app-server JSON-RPC schemas are unchanged.

Contract: [JSON Schema](SUSHIGPT_TELEMETRY.schema.json), [synthetic JSONL examples](SUSHIGPT_TELEMETRY.examples.jsonl). Required fields are present; unavailable values are `null`. Transport-specific fields are optional.

- `routing_decision`: one record after a prepared native V1/V2 spawn finishes. `requestedModel` is the explicit argument; `policyModel` is the optional rule/Jev candidate; `selectedModel` is the validated configuration after native role overrides. `source` and fixed `reasonCode` identify selection or fallback. `childThreadId` is null if spawning fails after preparation. Configuration/argument failures before preparation finishes have no decision record. No task names, role names, descriptions or errors are serialized.
- `source` is `native` (no policy candidate), `rule` (fixed rule), `jev` (validated classifier answer), `fallback` (a failure kept the native defaults; `reasonCode` names it) or `fallback_class` (a configured `agent_model_routing.jev.fallback_class` supplied the candidate after a recoverable classifier failure). With `fallback_class`, `reasonCode` stays the classifier's own failure reason (`uncertain`, `timeout`, `transport`, `http`, `oversized_response` or `invalid_response`) and `policyModel` is the fallback class's model. `missing_key`, `unsupported_input` and `unavailable_model` never use the fallback class and keep `source: fallback`. The matching `classifier_transition` still reports the classifier's own outcome.
- `response_usage`: one terminal record per visible HTTP/WebSocket stream acquisition attempt or local CLI model step. `attemptId` is local and unique. `requestId` comes from the native upstream request header when available; `responseId` is the observed provider response ID. `requestedModel` is the request model, while `executedModel` is populated only by native `ServerModel` metadata (including `OpenAI-Model`). Missing server metadata stays null; request configuration is not evidence of execution.
- `usage` contains only that response's observed token usage. `status: incomplete` means interruption/error/end before an observed completion and has null usage. A completed response can also have null usage. Input/output/total preserve reported zero; cache read/write and reasoning preserve positive numbers, but zero becomes null because the upstream parser already collapses missing details to zero. Negative values become null.
- The observer never copies input/output text, arbitrary metadata, pricing, cumulative thread counters or inherited parent usage. `decisionId` in usage is currently null; correlate `routing_decision.childThreadId` with `response_usage.threadId`. A fast child may emit usage before its decision record. The usage record's parent/root turn fields come from native request metadata and can be null.

Local CLI steps may add `transport: "claude_cli"`, `transportInstanceId` and `cliVersion` after the owned CLI initializes. `executedModel` comes from the model stream, never the configured preference. These fields identify the local transport, not its credential source. CLI session totals are not added to individual response usage. Task success additionally requires native tool results and child completion.

## Reader and aggregation rules

Tail only this dedicated file. Buffer incomplete UTF-8 lines. Each line is at most 4096 bytes including newline; the file is capped at 8 MiB. Require schemaVersion 1 and the closed field allowlist. Model/identifier strings are at most 256 bytes and restricted to ASCII letters, digits and `-_.:/@`; invalid values become null.

The bounded 1024-record queue never waits for disk. A full queue drops a record and increments `droppedRecords`, which the next written record reports. `sequence` increases for written records within one `processInstanceId`. Startup clears the file under its exclusive sibling `.sushigpt-telemetry.lock`. Each startup/rotation has a fresh `generationId`; rotation truncates in place and sequence continues. Treat truncation, process/generation changes, rising drop count and sequence gaps as possible coverage loss. A second writer for the same profile disables its diagnostics. File errors disable diagnostics without failing or changing the native request/spawn. Files are private on Unix; symlinks and hardlinks are rejected there.

The feed is best effort: abrupt shutdown, final queued records, final unreported drops or writer failure can lose data. No fsync durability or complete-history claim. Reset/rotation must not silently turn unknown history into a complete zero-cost total.

Deduplicate completed usage by provider `responseId` across native rawResponse and JSONL events and across parent/child history; use `recordId` for duplicate diagnostic delivery. Supplement missing fields from duplicates without adding tokens twice. Genuine observed responses with distinct IDs count separately. When responseId is absent, keep attempt/record identity but do not infer equivalence to other channels. Keep thread cumulative totals separate from own response sums. Filter to the selected root and known descendants; buffer out-of-order parent relationships within a bound. Missing fields contribute unknown coverage, never fabricated zero.

## Coverage and limits

Included: Responses HTTP and WebSocket streams through the core client, including Responses-based compaction and native V1/V2 child streams. Warmup calls are excluded. Retries visible at this client boundary get distinct attempt IDs; lower transport retries are not individually exposed. Endpoints without stream usage (including the classifier and other non-Responses services) are not measured. This feed does not measure voice/audio billing, Jev charges, money saved, hidden provider consumption or a hard budget. Read this feed directly from its dedicated JSONL file.

Synthetic tests cover native child correlation and role precedence in both spawn versions, observed versus missing server model, response-only usage, ambiguous token details, duplicate completion, interrupted attempts, queue loss, bounded rotation and writer lock/link rejection. No real model, Jev or voice service is needed.

## Classifier lifecycle

`classifier_transition` uses the existing v1 envelope and native `parentThreadId`
and `parentTurnId`. `attemptId` identifies one native spawn routing evaluation,
including explicit skips. It is not a child identity. Required payload fields:
`phase`, `reasonCode`, `requestStarted` (boolean), `recommendedModel` (nullable
bounded model key), and `httpStatusCode` (nullable). No task, class description,
endpoint, key, response body, or prompt is included.

`phase` is one of `skipped`, `request_started`, `succeeded`, `failed`, `cancelled`.
The start event occurs immediately before the transport invokes `send()`, after
local prerequisites/client construction. It proves a dispatch attempt, not receipt
by the remote service. `succeeded` means a validated classifier recommendation,
not child creation or successful model execution. `recommendedModel` is the
configured target model key, not the classifier service's model. Subsequent native
validation can still reject that recommendation. The routing decision reports final selection separately.

Explicit skips use the reasons `native` (no routing configuration),
`routing_off`, `control_invalid`, `explicit`, `full_history`, `rule_matched`,
`unsupported_input`, `unavailable_model`, `missing_key`, or `transport` (client
setup failed). `rules_only` and `jev_disabled` identify runtime restriction and
an absent or disabled classifier, respectively. Fixed rules take priority even if Jev is enabled.
`request_started` uses the matching reason; validated success uses `jev_selected`.
Failures use `transport`, `timeout`, `http`, `oversized_response`,
`invalid_response`, or `uncertain`. Only HTTP failures have `httpStatusCode`.

Optional answer fields (additive, schemaVersion unchanged): `choice` (nullable
class label, `abstain` included), `confidence` (nullable 0..=1), `probabilities`
(nullable object of class label to 0..=1) and `minConfidence` (nullable 0..=1,
the configured `agent_model_routing.jev.min_confidence` the answer was judged
against). They are set together, and only once the transport parsed and
validated an answer: on `succeeded` with `jev_selected`, and on `failed` with
`uncertain` (abstain, tie, or confidence below the minimum). They stay `null`
on `skipped`, `request_started`, `cancelled`, and on `failed` with
`missing_key`, `transport`, `timeout`, `http`, `oversized_response` or
`invalid_response`, where no validated answer exists. Labels are the operator's
bounded configured class names; no descriptions, task text or prompts are
included. The values describe the classifier's distribution for tuning classes
and `min_confidence`, not task success.
Dropping an unfinished routing future emits `cancelled`; this does not assert
that every user interruption drops that future. `requestStarted` distinguishes cancellation before
or after dispatch. Missing events remain unknown: telemetry is opt-in, bounded,
and may drop records, and abrupt process termination cannot run cancellation
cleanup. Do not infer a skip from a missing classifier node.
