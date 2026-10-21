# ATIF (Harbor Agent Trajectory Interchange Format)

`atif` reads and writes [ATIF](https://github.com/harbor-framework/harbor/blob/main/rfcs/0001-trajectory-format.md),
the trajectory format Harbor uses for evals and RL. Every schema version from `ATIF-v1.0` to `ATIF-v1.8` is
imported and can be exported. Exports of ledgers that did not come from ATIF default to **`ATIF-v1.7`**; pass
`--variant ATIF-v1.x` (or `1.x`) for any other version.

## Where the data lives

ATIF is an interchange document, not a harness store. A Harbor trial writes `agent/trajectory.json`; agents
that summarise or split their context also write sibling files in the same directory:

| file | meaning |
|---|---|
| `trajectory.json` | the main trajectory |
| `trajectory.summarization-<n>-<role>.json` | a subagent referenced by `subagent_trajectory_ref.trajectory_path` |
| `trajectory.cont-<n>.json` | a continuation named by `continued_trajectory_ref` |
| `images/…`, `audio/…` | media referenced by content parts (v1.6+, audio v1.8) |

The importer takes a single `.json` file, or a directory (every `*.json` with an ATIF `schema_version`, found
recursively, `trajectory.json` first).

## Variants (schema versions)

The variant is the `schema_version` string; the producer version is `agent.version`, recorded as
`agent.source.version` (and on every native record). `agent.name` is kept in `source.extra.agent_name`.

| variant | adds (cumulative) | seen from |
|---|---|---|
| `ATIF-v1.0` | agent, steps (`system`/`user`/`agent`), tool_calls, observation, metrics, final_metrics, step `extra` | Harbor validator tests |
| `ATIF-v1.1` | root `extra` | |
| `ATIF-v1.2` | observations on system steps | Harbor goose, hermes, cortex-code converters |
| `ATIF-v1.3` | `metrics.completion_token_ids` | |
| `ATIF-v1.4` | `metrics.prompt_token_ids` | |
| `ATIF-v1.5` | `agent.tool_definitions`, `step.is_copied_context` | openhands, swe-agent, rovodev converters; the RFC example |
| `ATIF-v1.6` | ContentPart arrays (text + image by path) in `message` and observation `content` | terminus-2, acp, cline, kimi-cli, trae |
| `ATIF-v1.7` | `subagent_trajectories` + `trajectory_id`, `tool_call.extra`, observation-result `extra`, `llm_call_count`, the `context_management` convention; `session_id` optional; refs resolve by `trajectory_id` or `trajectory_path` (a `session_id`-only ref is no longer valid) | most Harbor converters (claude-code, codex, opencode, gemini-cli, pi, …) |
| `ATIF-v1.8` | `audio` content parts | Harbor main (unreleased when written) |

## Import mapping

Every trajectory document is one ledger: the file's root, each embedded `subagent_trajectories` entry, and each
sibling file reached through a ref. Agent ids are `trajectory_id`, else `session_id`, else the file stem; a
collision (continuations share the session id) appends `:<file stem>`, an embedded collision is prefixed with the
parent id.

| ATIF | protoledger |
|---|---|
| root object (with `steps`/`subagent_trajectories` emptied) | a leading event `trajectory`: `settings` {`model` ← `agent.model_name`, `tools` ← `agent.tool_definitions`} (kind `native` when neither is set). Its native record is the root skeleton. |
| `final_metrics` | a trailing `usage` with `cumulative: true` (part 1 of the root record) |
| first step `timestamp` | `agent.created_at` |
| `session_id` | `source.native_id` |
| step | one native record, id `step-<step_id>`; events `step-<n>`, `step-<n>#1`, … all with the step's `timestamp` |
| `source: user` step | starts turn `turn-<step_id>`; `message` → `text` role `user` (or `text`/`media` per content part) |
| `source: system` step | `text` role `system` |
| `source: agent` step | `settings` {`model`, `effort`} when `model_name`/`reasoning_effort` changes; `reasoning` ← `reasoning_content`; `text` role `assistant` ← `message`; `tool_call` per tool call; group `step-<n>` on what the model emitted (reasoning, text, media, tool calls) |
| `tool_calls[]` | `tool_call` {`call_id` ← `tool_call_id`, `name` ← `function_name`, `input` ← `arguments`, `tool_type` (`function`, or `extra.tool_type`), `server` ← `extra.server`; `input_text` when `extra.input_text` is true} |
| observation result with `source_call_id` | `tool_result` {`call_id`, `output` (string, or `{type:text}`/`{type:media}` parts), `is_error`/`status` ← `extra.is_error`/`extra.status`} |
| observation result without `source_call_id` | `tool_result` without `call_id` (environment feedback not tied to a call, e.g. terminus-2) |
| `subagent_trajectory_ref[]` | `subagent` `start` and `end` {`agent` = child ledger id, `call_id`, `description` ← `extra.summary`, `agent_type`, `outcome`} |
| embedded `subagent_trajectories[i]` / ref'd sibling file | child ledger, `parent {agent, event: the subagent start, relation: subagent}`; `source.extra.embedded = i` or `source.extra.file` |
| `continued_trajectory_ref` (file present) | separate root ledger; links `continued-in` / `continued-from` |
| system step with `extra.context_management.type = "compaction"` | `compaction` `boundary` {`trigger`, `tokens_before`, `tokens_after` from the convention object} plus a `compaction` `summary` per observation content |
| `metrics` | `usage` {`input` = `prompt_tokens − cached_tokens − extra.cache_write_tokens`, `output` ← `completion_tokens`, `cache_read` ← `cached_tokens`, `cache_write`, `reasoning` ← `extra.reasoning_tokens`, `cost_usd`, `model`}, in the step's `group`, right after the step's own events and before its results (SPEC §3.1.1) |
| image/audio part whose file exists next to the trajectory | `media` {`media_type`, `data` (base64 of the file), `name`} |
| image/audio part by URL or missing file | `media` {`media_type`, `url`} or {`media_type`, `path`} |
| `is_copied_context: true` | `meta.atif.is_copied_context` on each event of the step |
| step with nothing neutral | one `native` event |
| `notes`, root/agent/step `extra`, `llm_call_count`, token ids, logprobs, `total_steps` | native only |

Neutral `usage.input` excludes cached tokens (ATIF's `prompt_tokens` includes them); export adds them back, so
the totals survive exactly.

## Exact self trip

ATIF is one JSON document, usually pretty-printed. The importer detects the layout and stores it in
`agent.source.extra.json`: indent (none, 2, 4, …), single-line separators (`", "` Python / `","` JavaScript),
`ensure_ascii`, escaped slashes, trailing newline, and Harbor's `format_trajectory_json` (numeric arrays such as
token ids on one line; terminus-2, trae, kimi-cli, junie). Any document no style reproduces (prettier output, the
hand-formatted RFC example) keeps its whole text as `native.raw` on the root event (`"json": "raw"`); export emits
it only when the rebuilt document is identical. Numbers keep their source spelling (an unchanged document is rebuilt from its records' own texts), and
explicit `null` fields are preserved. Image/audio files read at import are written back from `data`.

All 24 fixtures come back byte for byte.

## Export of other ledgers

Events with no ATIF native record are rendered into new steps; ledgers from ATIF that gained events keep their
native steps and get the new ones appended.

- `text` user → user step; `system` (and `developer`) → system step; assistant `text`, `reasoning`, `tool_call`
  and `media` of one message `group` → one agent step (without a group, a new step starts after tool results).
- `tool_result` → observation result in the step that holds the call (`is_error`/`status` → result `extra`).
- `subagent` start → `subagent_trajectory_ref` on that result; child ledgers are embedded in
  `subagent_trajectories` (v1.7+, `trajectory_id` = ledger id) or written as `trajectory.<child>.json` and
  referenced by `trajectory_path` (before v1.7). `end.outcome` → `ref.extra.outcome`.
- `usage` → step `metrics` (several in one step are summed and `llm_call_count` set); `cumulative` usage →
  `final_metrics`. First `settings` → `agent.model_name` / `tool_definitions`; later model/effort changes →
  step `model_name` / `reasoning_effort`; `system_prompt` → a system step.
- `compaction` → a system step with `extra.context_management {type: compaction, boundary: replace}` and the
  summary as observation content (the RFC's Section VII convention).
- `media` with `data` → a file under `images/` or `audio/` next to the trajectory, referenced by path.
- `agent.name` is the source format (`protoledger` when unknown), `agent.version` its version or
  `--target-version`; `session_id` the source session id (the ledger id before v1.7).

### Version conversion

The same pass moves native documents between versions (`--variant` on an ATIF ledger). Going down removes what
the target cannot hold and names it; going up to v1.7 gives `session_id`-only refs a `trajectory_id`
(`subagent-ref-unresolvable`). Non-object `arguments` are wrapped as `{"input": …}` whenever a document is
re-rendered. Exact copies are never altered.


## Branches (SPEC §2.3)

ATIF has no branch: a trajectory is one list of steps, read in order. Decision: **not expressible** — the exporter renders the active
path, names `abandoned-branch-not-rendered`, and carries the abandoned events on
the next step. Done for every variant (one exporter). The comparator now reads `after` (`<kind>.branch` costs what dropping the event costs, `<kind>.after` a wrong predecessor), so writing the abandoned branch as the conversation is no longer free: `protoledger/branch-rewind` via atif was 1 with the branch written inline (where the comparator could not see it) and is 13 with the active path rendered, its 4 abandoned events (13 facts) missing; written inline it would score 15 under the new comparator. With carry the trip still loses nothing (carried score 0).

## Carry

ATIF's `extra` objects are the natural carrier. With carry on (the default), each rendered step gets
`extra.protoledger = {v, h, events}` (SPEC §6) holding the events it represents plus the events ATIF cannot
express that followed it; the root gets `extra.protoledger.agent` with the ledger id, `created_at`, `meta` and
`links` (v1.1+). Events consumed by the root (first settings, cumulative usage) ride with the first step. On
import, an unedited step restores its carried events (in their original order); an edited step is imported fresh.
Step `extra` exists in every version, so carry works down to v1.0 (header carry needs v1.1).

## Loss codes

Import:

| code | when |
|---|---|
| `tool-arguments-not-object` | `arguments` is not an object (kept as given in `input`) |
| `content-part-unknown` | a content part whose `type` is not text/image/audio (native only) |
| `file-not-imported` | a file in the trajectory directory that no trajectory references |

Export (rendering):

| code | when |
|---|---|
| `reasoning-sealed-dropped`, `reasoning-redacted-dropped` | signed/encrypted or redacted reasoning |
| `settings-dropped`, `tools-dropped` | settings other than model/effort/tools/system prompt; tools that are not function definitions |
| `developer-as-system` | developer-role text written as a system step |
| `peer-message-fields-dropped` | peer messages are written as SPEC §3.1.6 wrapped text (user when incoming, assistant when outgoing), which the importer turns back into a `peer_message`; this code names the fields beyond direction, peer, network and text that the wrapper cannot hold |
| `tool-input-wrapped` | non-object tool input wrapped as `{"input": …}` |
| `tool-result-unmatched` | a result whose call is not in a rendered step (written without `source_call_id`) |
| `usage-unattached` | usage before any agent step |
| `usage-sum-unrepresentable` | two usages summed into one step's metrics whose sum is past what a JSON number holds (`f64::MAX + f64::MAX`); the earlier value is kept. No fixture reaches it; a unit test does (`extreme_spellings_neither_panic_nor_mis_sum`) |
| `compaction-preserved-dropped` | `compaction.preserved` has no ATIF field |
| `media-type-unsupported`, `media-dropped`, `media-path-relative` | media ATIF cannot hold, undecodable data, relative paths from another tool |
| `title-dropped`, `turn-dropped`, `error-dropped`, `file-change-dropped`, `app-dropped`, `native-dropped` | kinds with no ATIF form |
| `empty-trajectory-padded` | an empty ledger written with one empty system step (ATIF needs one) |
| `exact-text-fallback` | an unchanged document (every record its own) could not be rebuilt from its records' own texts (a piece has no text, or the rebuilt text does not parse to the document's value) and is written from the parsed value, so its layout, number spelling or escapes may differ from the source. No fixture reaches it; a unit test does (`an_unchanged_document_not_rebuilt_from_its_texts_names_the_fallback`) |

Export (version conversion): `extra-dropped` (<1.1), `system-observation-dropped` (<1.2), `token-ids-dropped`
(<1.3/<1.4), `tool-definitions-dropped`, `copied-context-dropped` (<1.5), `media-dropped`,
`content-parts-joined` (<1.6), `llm-call-count-dropped`, `tool-call-extra-dropped`, `observation-extra-dropped`,
`subagent-ref-unresolvable` (<1.7, or a session-only ref going to 1.7), `media-dropped` for audio (<1.8).

## Validation against Harbor

Exports were validated with Harbor's own `TrajectoryValidator` (pydantic models, image-path checks) from
harbor-framework/harbor `62db0c1`, installed in a throwaway Python 3.12 venv, plus a strict per-version field
check (Harbor's model accepts the union of all versions, so it cannot flag a v1.7 field in a v1.2 file).
Inputs: the 24 ATIF fixtures, the 7 Slack fixtures and the 3 reference protoledger ledgers, each exported to every
version v1.0–v1.8 and to the default. **429 of 441 files are valid.** The 12 invalid ones are all inherited from
inputs that are themselves invalid: exact copies of a letta fixture whose tool `arguments` is a list, and image
files referenced by fixtures that never shipped them (`pi_image_*.png`, letta's `images/*.png`). Every export
of a valid input validates.

## Fixtures

24 cases across every version: Harbor golden trajectories (terminus-2 timeout v1.6, invalid-json v1.8, a
four-file context-summarisation run and a continuation pair v1.8), Harbor's committed Claude Code 2.1.178 run
(v1.7), the RFC example (v1.5, raw layout), Harbor validator test data (v1.0, v1.6), outputs of Harbor's own
converters captured from its unit tests (goose and cortex-code v1.2, rovodev v1.5, acp v1.6, kimi-code, pi and
strands v1.7, claude-code with explicit nulls v1.8), two letta-ai/trajectory fixtures (prettier layout, embedded
and file-ref subagents), and labelled synthetics for v1.1, v1.3, v1.4, a full v1.7 feature set and v1.8 audio.

## Verified and unverified

- Verified: byte-exact self trips for all fixtures; Harbor validation of exports; carry round trips clean for
  every fixture and the reference ledgers (ATIF → P → ATIF and foreign → ATIF → P).
- Unverified: real-world v1.1, v1.3, v1.4 files (none found; synthetic only); Harbor's `trajectory_path` refs to
  S3 or database locations (kept as references, not fetched); ATIF documents larger than a few hundred KB
  (layout detection may render the document up to 24 times).

## Prefix stability (SPEC §9.3)

Not tested for prefix stability: a trajectory is one JSON document, with no record boundary to cut at.
