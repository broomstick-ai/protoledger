# OpenHands

OpenHands (`OpenHands/OpenHands`, formerly All-Hands-AI) has stored
conversations in two generations. Module: `src/formats/openhands.rs`.
Research references: OpenHands tag `1.0.0` (`0cb27a4`, the last with the V0
Python code) and `software-agent-sdk` `bf9b605` (1.55.1).

## Storage and variants

| variant | producer | where | shape |
|---|---|---|---|
| `v0-events` | OpenHands ≤ 1.0 (V0 conversations; the V0 code was removed after 1.0) | `~/.openhands/sessions/<sid>/` (`users/<uid>/conversations/<sid>/` in SaaS): `events/<n>.json`, `metadata.json`, optional `event_cache/<a>-<b>.json`, `agent_state.pkl` | one event dict per file, Python `json.dumps` (separators `", "`/`": "`, `ensure_ascii`), no newline; `id` int, `action` or `observation` type tag, `args` / `content`+`extras`, `cause`, `tool_call_metadata` (function name, call id, full litellm `model_response`), `llm_metrics` (cumulative) |
| `v0-trajectory` | OpenHands ≤ 1.0 | `save_trajectory_path` file, `/api/conversations/{id}/trajectory`, events API page | the same dicts in one JSON file: a bare array (`json.dump(indent=4)`), `{"trajectory": [...]}` or `{"events": [...], "has_more"}` |
| `v1-events` | `software-agent-sdk` ≥ 1.0.0a1 (OpenHands ≥ 1.0, CLI, agent server, Agent Canvas; 1.55 today) | `<base>/<conversation id hex>/` with `base_state.json`, `events/event-<idx:05d>-<uuid>.json`, `events/.eventlog.lock`, `events/.eventlog-len-<N>.marker`; bases: `~/.openhands/conversations` (CLI), `workspace/conversations` (agent server), `~/.openhands/agent-canvas/conversations` | pydantic `model_dump_json(exclude_none=True)`, compact, no newline; `kind` (class name) last; `parent_id` from SDK 1.31 |
| `v1-api` | agent server | `/api/conversations/{id}/events/search` page or a saved list | `{"items": [...], "next_page_id"}`, a bare array, or JSONL |

Detection is by content: V0 dicts carry a string `action`/`observation`, V1
events a `kind`; directories by `events/<n>.json` versus `event-*.json` /
`base_state.json`. **Producer version:** V0 records `openhands_version` (and
`agent_class`) on its first `system` action, which goes to
`agent.source.version` and every event's `native.version`. V1 records no
version anywhere; field presence gives a lower bound (`parent_id` ⇒ ≥ 1.31,
`ObservationEvent.extended_content` ⇒ ≥ 1.34, `dynamic_context` ⇒ ≥ 1.11.3).

One conversation directory (or file) is one ledger; its id is the directory
(or file) name. Files in a conversation directory that are not events are
kept verbatim in `agent.source.extra.files` (base64 in `files_b64` when not
UTF-8), so `event_cache/`, lock files, `meta.json` or `observations/` come
back byte for byte.

## Mapping

Event ids are the native ids (`<n>` for V0, the UUID for V1; `<id>#<k>` for
part k). V1 `parent_id`, when it is not the previous event, becomes `after`.
LLM responses group events: V0 by `model_response.id`, V1 by
`llm_response_id`. OpenHands writes naive local times (`datetime.isoformat()`);
they are read as UTC and marked `Z` (SPEC §3.1.7). User or assistant text that
is a SPEC §3.1.6 `<peer-message …>` wrapper is imported as `peer_message`.
Usage follows SPEC §3.1.1: `input` is prompt tokens minus cache reads (and
writes, for metrics), one per-response `usage` after the last action of that
response, and `llm_metrics` / `stats` totals as `cumulative: true`.

**V0**

| record | events |
|---|---|
| `metadata.json` | `settings` {model: llm_model}; `title`; `usage` {cumulative: true, cost_usd: accumulated_cost} |
| `system` action | `settings` {system_prompt, tools, agent: agent_class} |
| `message` action | `text` (user / assistant / system by `source`) + `media` for `image_urls` |
| tool actions (`run`, `run_ipython`, `read`, `write`, `edit`, `browse*`, `call_tool_mcp`, `delegate`, `task_tracking`, or any action with `tool_call_metadata`) | `text` assistant (`thought`), `tool_call` {call_id: tool_call_id or `oh_<id>`, name: function_name or the action type, input: the model's arguments (else `args`), tool_type shell/mcp/function}; once per response: `reasoning` (`reasoning_content`), `usage` (per-response tokens); `usage` {cumulative} from `llm_metrics`; `delegate` adds `subagent` {start} |
| `finish` | `text` assistant (`final_thought`) + `tool_call` when called as a tool |
| `think` | `tool_call`, or `reasoning` when not a tool call |
| `condensation` | `compaction` {summary or boundary, forgotten} |
| observation with a `cause` tool action | `tool_result` {is_error: `error`, `user_rejected` (status rejected) or `success: false`}; `edit` → `file_change` {create/edit, diff, content}; `write` → `file_change`; `delegate` → `subagent` {end} |
| `agent_state_changed` | `turn` {start on running; end ok on awaiting_user_input/finished, error, interrupted on paused/stopped/rejected} |
| `error` without a cause | `error` {kind: error_id} |
| `recall`, `null`, `change_agent_state`, `condensation_request`, … | `native` |

**V1**

| event | events |
|---|---|
| `base_state.json` | `settings` {model: agent.llm.model, cwd: workspace.working_dir}; `usage` {cumulative} from `stats.usage_to_metrics` |
| `SystemPromptEvent` | `settings` {system_prompt, tools} |
| `MessageEvent` | `reasoning` (`reasoning_content`, `thinking_blocks` with Anthropic signatures, `responses_reasoning_item` with OpenAI encrypted content), `text` by `llm_message.role`, `media` for image URLs (`data:` URLs become inline data), `extended_content` as text |
| `ActionEvent` | `reasoning` (first event of a response only), `text` assistant (`thought`), `tool_call` {call_id, name: tool_name, input: parsed `tool_call.arguments`, tool_type shell/mcp/function} |
| `ObservationEvent` | `tool_result` {output: text or parts, is_error}; `FileEditorObservation` edits → `file_change` |
| `UserRejectObservation` | `tool_result` {is_error, status: rejected} |
| `AgentErrorEvent` | `tool_result` {is_error} for a call, else `error` |
| `ConversationErrorEvent` | `error` {kind: code, message: detail} |
| `Condensation`, `CondensationSummaryEvent` | `compaction` |
| `PauseEvent`, `InterruptEvent` | `turn` {end, interrupted} |
| `ACPToolCallEvent` | `tool_call` + `tool_result` |
| `ConversationStateUpdateEvent` key `stats` | `usage` {cumulative} |
| others (`LLMCompletionLogEvent`, `HookExecutionEvent`, `SecurityAnalysisEvent`, state updates, `CondensationRequest`, …) | `native` |

## Export

Native records found intact are written back unchanged: V0 event files with a
Python `json.dumps` re-implementation (`py_dumps`) or their raw text, V0
trajectory files in their detected style (`json.dump(indent=4)` and friends)
or raw, V1 files compact. Self trip is byte-exact on all 9 fixtures.

Everything else is written as a **V1 conversation**
`<uuid5 hex>/{base_state.json, events/event-NNNNN-<uuid>.json, events/.eventlog.lock, events/.eventlog-len-N.marker}`,
event ids UUIDv5 of the event id:

- user/system text and media → `MessageEvent` (role user/system; images as
  `data:` URLs); assistant text, reasoning and tool calls of one group →
  one `MessageEvent`, or one `ActionEvent` per tool call (the first holding
  the thought, `reasoning_content` and Anthropic `thinking_blocks`), with
  `llm_response_id` from the group. Actions are written without an `action`
  payload (non-executable), since foreign tools are not OpenHands tools.
- `tool_result` → `ObservationEvent` with a generic `MCPToolObservation`
  (content + is_error) pointing at its `ActionEvent`.
- `settings.system_prompt` → `SystemPromptEvent`; a ledger without one gets an
  empty `SystemPromptEvent` first, because the SDK refuses to run a
  conversation whose first three events hold a user message but no prompt.
  Model and cwd go to `base_state.json` (`settings-in-base-state`).
- `compaction` with a summary → `CondensationSummaryEvent`; `error` →
  `ConversationErrorEvent`; an interrupted turn end → `InterruptEvent`.
- `peer_message` → a user `MessageEvent` (incoming) or assistant text
  (outgoing) holding the SPEC §3.1.6 wrapper.
- Dropped, with codes: `compaction-boundary-dropped`, `usage-dropped`,
  `title-dropped`, `turn-dropped`, `subagent-event-dropped`,
  `file-change-dropped`, `app-dropped`,
  `native-dropped`, `media-dropped`, `reasoning-sealed-dropped` (encrypted
  reasoning from a vendor other than Anthropic and OpenAI, or a second OpenAI
  reasoning item in one response, is written as plain text). OpenAI
  encrypted reasoning and reasoning summaries go in the response's
  `responses_reasoning_item` (`summary`, `content`, `encrypted_content`,
  SPEC §3.1.5), which the importer reads back. A
  subagent ledger becomes its own conversation
  (`subagent-separate-conversation`). Events added to a V0 ledger are not
  written (`v0-append-unsupported`: OpenHands no longer runs V0).
- `parent_id` chains every event to the previous one, as SDK ≥ 1.31 writes.


## Branches (SPEC §2.3)

OpenHands V1 events carry `parent_id` (SDK >= 1.31), but nothing shows the SDK rebuilding history along it rather than in event-index order, and V0 has no parent at all, so a branch is treated as not expressible. Decision: **not expressible** — the exporter renders the active
path, names `abandoned-branch-not-rendered`, and carries the abandoned events on
the next record. Done for every foreign ledger and every foreign event appended to a native one (both rendered as V1 events). The comparator now reads `after` (`<kind>.branch` costs what dropping the event costs, `<kind>.after` a wrong predecessor), so writing the abandoned branch as the conversation is no longer free: `protoledger/branch-rewind` via openhands was 10 with the branch written inline (where the comparator could not see it) and is 19 with the active path rendered, its 4 abandoned events (13 facts) missing; written inline it would score 24 under the new comparator. With carry the trip still loses nothing (carried score 0).

## Carry

Partly. V1 events are `extra="forbid"`, but `Message` (`llm_message`) and
`MessageToolCall` (`tool_call`) ignore unknown keys, and `ConversationState`
(`base_state.json`) ignores them too. So the carrier sits in
`llm_message.protoledger` (MessageEvent) or `tool_call.protoledger`
(ActionEvent). Records that cannot hold one (observations, system prompts,
errors, condensations, the second and later actions of a response) have
their events carried by the next record that can, and are listed in that
carrier's `covers` so a carried re-import skips them instead of importing
them twice. The ledger's `agent`, and events no event could hold, ride in
`base_state.json`'s top-level `protoledger` key; a `base_state.json` carrying
an agent is treated as derived on re-import. V0 is never written for foreign
ledgers, so it carries nothing.

## Verified

- Self trip byte-exact on all 9 fixtures: 4 real V0 (OpenHands' own replay
  trajectories, harbor's 0.58.0 trajectory, and the native per-file layout of
  a real stream), 1 written by `openhands-sdk` 1.55.1 itself, 1 real V1 slice
  from the SDK's tests, 1 minimal V1, 2 synthetic API files from trajectory.
- Loadability: every openhands export of the pi, omp and letta-code fixtures
  (27 conversations, carry on and off) was opened with `openhands-sdk`
  1.55.1's `Conversation(..., persistence_dir, conversation_id)` and
  **resumed**: a new user message was sent and `run()` appended the reply.
- pi/omp/letta-code → openhands → back with carry is byte-exact on every
  fixture; every openhands fixture → pi/omp/letta-code → openhands likewise.

Unverified: real V1 conversations from a user's machine (none under a usable
license); V0 `event_cache` pages and `agent_state.pkl` are kept verbatim, not
read; tool payloads that need `openhands-tools` validate only where that
package is installed; delegated sub-conversations are not linked.

## Prefix stability (SPEC §9.3)

Not tested for prefix stability: a conversation is one JSON file per event, or one JSON document, not lines; the line test in `tests/prefix.rs` does not cut a directory of files.
