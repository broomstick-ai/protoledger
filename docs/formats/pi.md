# pi

pi is Mario Zechner's coding agent (`badlogic/pi-mono`, now published as
`earendil-works/pi`, npm `@earendil-works/pi-coding-agent`; before 1.0
`@mariozechner/pi-coding-agent`). The adapter shares its code with
[omp](omp.md), a pi fork: `src/formats/pi_session.rs`.

## Storage

```
~/.pi/agent/sessions/--<cwd with / \ : replaced by ->--/<ISO timestamp with : . → ->_<session id>.jsonl
```

`$PI_CODING_AGENT_DIR` replaces `~/.pi/agent`. One file per session. Line 1 is
the `session` header; every later line is an entry. pi appends with
`JSON.stringify(entry) + "\n"` and rewrites the whole file only when it
migrates an old version. The input may be one file or a directory (every
`*.jsonl` under it is a session).

## Variants

Detected from the header's `version` (session schema version; pi records no
producer version anywhere in the file).

| variant | session version | pi versions | shape |
|---|---|---|---|
| `v1` | absent | < 0.31.0 | linear entries **without** `id`/`parentId`; the header also carries `provider`, `modelId`, `thinkingLevel` (and `branchedFrom`); compaction stores `firstKeptEntryIndex` |
| `v2` | 2 | 0.31.0 – 0.34.x | tree: every entry has `id` (8 hex) and `parentId`; extension messages are `role: "hookMessage"` |
| `v3` | 3 | ≥ 0.35.0 (1.x today) | `hookMessage` renamed `custom`. Additive since: `session_info` (0.44), `usage` entries and `system` messages with `sections`/`toolsAdded` (0.86), `context_edit` (0.87), assistant `thinkingLevel` (0.99) |

Dates were taken from `git log -S` over pi-mono (`CURRENT_SESSION_VERSION`,
entry type names) and the first tag containing each commit. pi itself migrates
v1/v2 to v3 on load; the adapter imports all three as they are and exports v3.

## Branches

A pi session is a **tree**. Branching (`/tree`, `branch()`, `branchWithSummary`,
`resetLeaf()`) appends new children to an earlier entry; nothing is rewritten.
The active position (the leaf) is never persisted: on load it is the last entry
in the file.

Decision: **the active path is the agent; every other branch is its own fork
ledger.** The root ledger (`agent.id` = session id) holds the header and the
entries on the path from the root to the last entry, in file order. Each branch
that leaves that path becomes a ledger `<session id>@<first entry id>` with
`agent.parent = {agent: <ledger it leaves>, event: <fork-point entry id>,
relation: "fork"}` (SPEC §1.1: a forked branch is its own file). Inside a
branch the deepest leaf (latest in file order) is that branch's own path, and
branches off it nest the same way. Additional roots (`resetLeaf`) become forks
with no fork event. So every ledger is linear and `after` is never needed, a
consumer that reads only the root ledger sees exactly the context pi would
send next, and abandoned attempts are kept rather than flattened into it.

`agent.source.extra.lines` (present only when there are branches or blank
lines) lists the physical line of each record in that ledger, so the exporter
interleaves the ledgers back into one file byte for byte. Without it (a ledger
that came back through another harness without its header), records are placed
in ledger order with each fork right after its fork point and the root
ledger's last record kept last, so the leaf stays the leaf.

Records that cannot join the tree (v2+ entries without an id, malformed lines,
cycles) stay in the root ledger at their file position. Malformed lines are
kept as `native` events with `native.raw` (pi skips them on load).


### Export of a rewound ledger (SPEC §2.3)

pi can express a branch (the session tree above), but the importer turns every branch off the active path into its own fork ledger, so a rewound ledger written as a pi branch comes back as two ledgers, not as one ledger with `after`. Decision: **not expressible** — the exporter renders the active
path, names `abandoned-branch-not-rendered`, and carries the abandoned events on
the next record. Done for every session version (the shared pi session writer). The comparator now reads `after` (`<kind>.branch` costs what dropping the event costs, `<kind>.after` a wrong predecessor), so writing the abandoned branch as the conversation is no longer free: `protoledger/branch-rewind` via pi was 8 with the branch written inline (where the comparator could not see it) and is 17 with the active path rendered, its 4 abandoned events (13 facts) missing; written inline it would score 22 under the new comparator. With carry the trip still loses nothing (carried score 0).

## Mapping

Event ids are entry ids (`<id>`, `<id>#<n>` for part n); v1 entries and other
id-less lines use `@<byte offset>`. Every record's JSON is in `native.record`
on its first event, with `native.raw` when `JSON.stringify` and serde_json
would disagree (never seen so far). Events from an assistant message share
`group` = its `responseId` when pi recorded one, else the entry id
(SPEC §3.1.4); multi-part user messages share the entry id. User or assistant
text that is a SPEC §3.1.6 `<peer-message …>` wrapper is imported as the
`peer_message` it stands for.

| pi record | events |
|---|---|
| `session` header | `settings` {cwd; v1: model, provider, effort} (`native` when it states nothing but an empty `cwd`, which the exporter writes for a ledger that names none, SPEC §3.1.3); `parentSession`/`branchedFrom` → header link `forked-from` |
| message `user` | `text` (role user) per text block, `media` per image |
| message `assistant` | per block: `thinking` → `reasoning` {text, redacted, sealed {vendor `anthropic`/kind `signature`, or vendor `openai`/kind `encrypted` for openai-* apis (SPEC §3.1.5), data: thinkingSignature}}; `text` → `text` assistant; `toolCall` → `tool_call` {call_id, name, input: arguments, tool_type: `shell` for `bash`, else `function`}; then `usage` {model, input, output, cache_read, cache_write, reasoning, cost_usd: cost.total} (pi-ai already counts input without cache reads and output with reasoning, as SPEC §3.1.1 asks; the provider is a setting, not a usage fact; a block whose counts and cost are all zero states nothing and makes no `usage`, as pi writes one when a provider reports none); `stopReason: error` → `error`; `aborted` → `turn` {end, aborted} |
| message `toolResult` | `tool_result` {call_id, output (string, or parts with media), is_error}; `edit` with `details.diff` → `file_change` {op: edit, diff}; `write` → `file_change` {op: create, content} |
| message `system` | `settings` {system_prompt: sections + content, tools: toolsAdded} |
| message `bashExecution` | `text` user = pi's `bashExecutionToText` (what the model sees); audience `record` when `excludeFromContext` |
| message `custom` / `hookMessage`, entry `custom_message` | `text` / `media`, role user (pi sends them to the model as user content) |
| `branch_summary` | `text` user with pi's branch-summary wrapper (+ `usage`) |
| `compaction` | `compaction` {phase: summary, summary, tokens_before, preserved: event ids from `firstKeptEntryId` (v1: `firstKeptEntryIndex`) up to it} (+ `usage`) |
| `model_change` | `settings` {model: modelId, provider} |
| `thinking_level_change` | `settings` {effort} |
| `session_info` | `title` |
| `usage` | `usage` {purpose: kind} |
| `custom`, `label`, `context_edit`, unknown | `native` |

## Export

pi native records found intact are written back unchanged (self trip is byte
exact on every fixture). Foreign ledgers are rendered as a v3 session:
a header (`id` = UUIDv5 of the ledger id, `cwd` from the first settings),
then one entry per message, chained by `parentId`, with entry ids
`short_id(<ledger id>/<event id>, 8)`. Consecutive assistant-side events of one
`group` (reasoning, text, tool calls, usage) become one assistant message;
`api` is derived from the provider (when no settings name one, the vendor of a
signed reasoning block's `sealed`, SPEC §3.1.5). A foreign root ledger is written as
`<created_at>_<uuid>.jsonl`; fork ledgers go into their parent's file as
branches; a subagent ledger is written as its own session file with
`parentSession` naming its parent's file (pi has no subagents).

A `peer_message` is rendered as SPEC §3.1.6 wrapped text: a user message when
incoming, assistant text when outgoing, which the importer turns back into a
`peer_message`.

Loss codes (export): `reasoning-sealed-dropped` (encrypted reasoning from
another API), `media-dropped` (images without inline data, assistant media),
`tool-input-wrapped` (non-object tool input → `{input}`), `settings-partial`
(sandbox, approval, ...), `compaction-boundary-dropped` (a boundary without a
summary), `usage-cumulative-dropped` (running totals; pi records per-response
usage only), `turn-dropped`, `subagent-event-dropped`, `file-change-dropped`,
`native-dropped`, `subagent-separate-session`.
`error` and `app` events become `custom` entries (`protoledger.error`,
`protoledger.app`), which pi keeps but does not send to the model.

Import loss codes: `malformed-line`, `record-native-only` (counts records kept
only as `native`).

## Carry

Yes. pi does not validate entries and keeps unknown fields when it rewrites a
file, so every rendered entry carries `protoledger: {v, h, events}` at the top
level; the header (and the first entry of a fork ledger) also carries the
ledger's `agent`. Events that render to nothing ride on the record being built
or on a `custom` entry with `customType: "protoledger.carry"`. Re-import with
`restore_carried` restores them when the record's hash still matches.
Carried events are parsed from text, not `serde_json::from_value`, which
would turn arbitrary-precision numbers (`0.000009`) into f64 (`9e-6`).

## Verified

- Self trip byte-exact on all 6 fixtures (v1 real excerpts, v2 and v3
  generated by pi's own `SessionManager` 0.34.2 and 1.3.0, trajectory's two).
- Exports load in the real harness: every pi export of the omp fixtures, and
  every fixture, opened with `SessionManager.open` + `buildSessionContext` +
  `convertToLlm` from `@earendil-works/pi-coding-agent@1.3.0` without errors,
  with the expected roles, model and session name.
- pi → omp → pi with carry comes back byte-exact on every fixture.

Unverified: sessions written by pi 0.35–0.85 (no samples); `context_edit` is
kept native and not applied; images in omp's blob store are not resolved.

## Prefix stability (SPEC §9.3)

Not prefix-stable where a session holds a branch. The importer makes the session tree's active path the agent, read back from the latest leaf, and every other branch its own fork ledger (Branches, above). A prefix that ends on another branch therefore has another active path, and its events are not the first events of the whole import. `tests/prefix.rs` measures this on every line of every fixture: 9 of 132 (file, k) pairs differ, all in the two fixtures that hold a branch (`v3-branches`, `v3-cleanup`); the four without one are stable. The count is pinned there. Making pi prefix-stable means importing the tree as one ledger with `after` (SPEC §2.3) instead of fork ledgers, which is a change to this adapter's design, not done here.
