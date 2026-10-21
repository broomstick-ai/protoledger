# deepagents (LangChain Deep Agents)

Adapter: `src/formats/deepagents/` (`store.rs` import/export, `msgpack.rs`
a MessagePack codec for LangGraph's `JsonPlusSerializer`). Format id:
`deepagents`. Needs the `sqlite` feature.

## Storage

| producer | store |
|---|---|
| deepagents-cli 0.0.13 – 0.0.48 | `~/.deepagents/sessions.db` |
| deepagents-cli ≥ 0.0.49, deepagents-code (`dcode`) | `$DEEPAGENTS_HOME/.state/sessions.db` (default `~/.deepagents/.state/`) |

The coding agent moved from `deepagents-cli` to `deepagents-code` at cli
0.1.0 (2026-05-15). The store is a LangGraph SQLite checkpointer
(`langgraph-checkpoint-sqlite`): tables `checkpoints` and `writes`, plus
dcode's optional `dcode_thread_names`, `dcode_btw_costs`,
`dcode_thread_workspaces`, `dcode_workspace_snapshots`. Thread ids are UUID7
strings (8 hex chars before cli 0.0.33). Import takes the `.db` file or a
directory holding any number of stores.

## Variants and versions

| variant | producers | shape |
|---|---|---|
| `checkpoint-full` (import) | deepagents < 0.6 (langgraph < 1.2) | `messages`, `files`, `todos` stored whole in every checkpoint's `channel_values` |
| `checkpoint-delta` (import, export) | deepagents ≥ 0.6.0, deepagents-code (langgraph ≥ 1.2) | `messages` and `files` are DeltaChannels: each write row holds a delta; a checkpoint holds an ext-7 snapshot every 50 updates. `writes.task_path` exists from checkpoint-sqlite 3.1.2 |

Detection: SQLite with `checkpoints` and `writes`; the variant is
`checkpoint-delta` when any checkpoint's metadata has
`counters_since_delta_snapshot`. No producer version is recorded in the
store, so `agent.source.version` is unset. Checkpoint blobs are format v4
(v2 in hand-built stores); `type` is `msgpack` (`json` in pre-msgpack stores
is also read; `pickle` is not).

## Reconstruction

Per `(thread_id, checkpoint_ns)`: the latest checkpoint (LangGraph's
`ORDER BY checkpoint_id DESC`) and its parent chain, oldest first. For each
checkpoint: a `messages` value held in it (a full list or a delta snapshot)
seeds messages not seen yet; then its writes, sorted by
`(task_path, task_id, idx)`, are replayed. Messages are deduplicated by
`id`, so full-state and delta stores give the same events (the fixtures show
it). This is the reconstruction LangGraph's `_delta.py` performs, except
that history is kept: a message later removed stays in the ledger.

## Mapping

| store | protoledger |
|---|---|
| a `(thread, '')` chain | a root ledger, id = thread id |
| a `tools:<task>` namespace | a ledger `<thread>/tools:<task>` with `agent.parent{relation: subagent, event: the parent's tool_call}`; the call is found through the parent's ToolMessage written by task `<task>` |
| run metadata (`agent_name` / `lc_agent_name`, `cwd`, `git_branch`) | `settings{cwd, git_branch, agent}` |
| `dcode_thread_names.name` | `title` |
| HumanMessage / SystemMessage | `text{role: user ‖ system}`, image blocks → `media` |
| AIMessage | `reasoning` (`thinking` blocks with `signature` → `sealed{kind: signature, vendor: anthropic}`, `redacted_thinking` → anthropic encrypted (SPEC §3.1.5: both blocks are Anthropic's), `reasoning` blocks {`reasoning` → text, `summary[].text` → summary, `encrypted_content` → `sealed{vendor: openai, kind: encrypted}`}, `additional_kwargs.reasoning_content`), `text`, `tool_call` per `tool_calls`, `usage` from `usage_metadata` (`input` = `input_tokens` − `input_token_details.cache_read` − `cache_creation`, SPEC §3.1.1: LangChain's `input_tokens` counts every input token; export adds them back; model from `response_metadata.model_name`); `group` = message id |
| ToolMessage | `tool_result{call_id: tool_call_id, output, is_error: status == "error"}` |
| `RemoveMessage(REMOVE_ALL_MESSAGES)`, `Overwrite(...)` | `compaction{phase: boundary, trigger: auto}` |
| `files` write `{path: FileData ‖ None}` | `file_change{path, op: create ‖ edit ‖ delete, content}` |
| `todos`, `__pregel_tasks`, `branch:to:*`, other channels | not events (kept in the rows) |

Event ids: the LangChain message id (`#n` parts); `<checkpoint>/<task>/<idx>/<n>`
for id-less items; `<checkpoint>/<task>/<idx>:<path>` for file changes. `ts`
is the timestamp of the checkpoint that committed the write.

## Self trip

**Not byte-exact** (`expect.json` `self_exact:false`): export rebuilds the
store from the captured schema text, every other table's rows, and the
ledger's own `checkpoints` and `writes` rows (kept in
`agent.source.extra.rows`, blobs byte for byte). Verified with `sqlite3` on
all six fixtures: identical schema text and identical rows, blobs included.
Checkpoints off the latest chain (time travel, `update_state` forks) are kept
as rows but give no events (`branch-dropped`).

If events were appended to a deepagents ledger after its native ones, export
writes the native rows and continues the thread with new checkpoints after
its latest one.

## Export of foreign ledgers

Writes `sessions.db` in the `checkpoint-delta` shape LangGraph 1.2 writes:
the checkpoint-sqlite 3.1.2 DDL, one thread per root ledger (id = the ledger
id when it is a plain id, else a UUID v5), subagent ledgers as
`tools:<task>` namespaces of their parent's thread, one checkpoint per step
(a human message, one AI message built from the parts sharing a `group`, a
tool message, a file change), the step's delta written against the
checkpoint before the next, a final checkpoint with no writes, UUID v6
checkpoint ids ordered by event time, dcode run metadata (`agent_name`,
`cwd`, `git_branch`, `updated_at`), and the title in `dcode_thread_names`.
Messages are serialised as LangChain does (`[module, class, model_dump(),
"model_validate_json"]` in msgpack ext 5).

**Verified with LangGraph:** exports of droid, openclaw and hermes fixtures
were opened with `SqliteSaver` + `create_deep_agent` (deepagents 0.7.28,
langgraph 1.2.16); `get_state` returned the expected messages and the
thread was continued with `invoke`. A thread continued that way and imported
again gives back the carried events plus the new turn; exporting that to
droid reproduced the original droid files exactly with the new turn appended.

| loss code | when |
|---|---|
| `reasoning-sealed-dropped` | encrypted reasoning other than OpenAI's (signatures go in `thinking`, OpenAI encrypted content in a `reasoning` block's `encrypted_content`, summaries as `summary_text` items) |
| `media-dropped` | media without data or URL; non-text tool output |
| `tool-input-text-wrapped` | raw or non-object tool input becomes `{"input": …}` |
| `compaction-summary-as-human` | a compaction summary becomes a HumanMessage |
| `peer-message-fields-dropped` | peer messages are written as SPEC §3.1.6 wrapped text (user when incoming, assistant when outgoing), which the importer turns back into a `peer_message`; this code names the fields beyond direction, peer, network and text that the wrapper cannot hold (a HumanMessage when incoming, an AIMessage when outgoing) |
| `record-only-text-shown` | text the model did not see becomes a message |
| `file-change-without-content` | a diff-only file change cannot set file content |
| `compaction-boundary-dropped`, `subagent-marker-dropped`, `error-dropped`, `app-dropped`, `foreign-native-dropped` | no LangGraph state holds it |
| `branch-dropped` (import) | checkpoints off the latest chain |
| `message-removal-not-represented` (import) | a RemoveMessage of a single message |


## Branches (SPEC §2.3)

A LangGraph thread's checkpoints can fork, but the `messages` channel this adapter writes is one list read in order, so the format as written here cannot express a branch. Decision: **not expressible** — the exporter renders the active
path, names `abandoned-branch-not-rendered`, and carries the abandoned events on
the next step. Done for both checkpoint variants (one writer). The comparator now reads `after` (`<kind>.branch` costs what dropping the event costs, `<kind>.after` a wrong predecessor), so writing the abandoned branch as the conversation is no longer free: `protoledger/branch-rewind` via deepagents was 16 with the branch written inline (where the comparator could not see it) and is 21 with the active path rendered, its 4 abandoned events (13 facts) missing; written inline it would score 30 under the new comparator. With carry the trip still loses nothing (carried score 0).

## Carry

Yes, in checkpoint `metadata` (JSON that LangGraph and dcode read by named
keys, never sent to a model): each rendered checkpoint's metadata holds a
`protoledger` carrier with the events its writes stand for, hashed over the
decoded write values; the first checkpoint also carries the agent header.
Rows rendered from carriers are not kept as native rows on re-import, so a
continued thread re-exports without duplicates.

## Unverified

- Stores written under `langgraph dev`, and provider content blocks not seen
  in the fixtures (`server_tool_use`, citations).
- Message field sets of langchain-core releases other than 1.6.12.
- Writing into a store while dcode holds a thread lease.

## Prefix stability (SPEC §9.3)

Not tested for prefix stability: the store is a SQLite database, not lines.
