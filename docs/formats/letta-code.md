# Letta Code

Letta Code (`letta-ai/letta-code`, npm `@letta-ai/letta-code`) is Letta's
coding-agent CLI. Its conversation history lives in a Letta backend (Letta Cloud
or a self-hosted server by default, nothing local), so the local, harness-owned
record of a thread is the **client transcript** the CLI appends for its
reflection/memory pipeline. The experimental **local backend** keeps a real
on-disk history too; the adapter reads both.

Module: `src/formats/letta_code.rs` (+ the shared `pi_session.rs` for the local
backend). Research notes behind this page: letta-code `69b7f82c` (v0.34.16+4).

## Storage

```
~/.letta/transcripts/<agent id>/<conversation id | default>/transcript.jsonl   ($LETTA_TRANSCRIPT_ROOT)
~/.letta/transcripts/<agent id>/<conversation id>/state.json                    reflection bookkeeping
~/.letta/lc-local-backend/conversations/<base64url key>/messages.jsonl          local backend ($LETTA_LOCAL_BACKEND_DIR)
                                                   .../manifest.json, conversation.json, system-prompt.json
```

The input may be a `transcript.jsonl`, any directory above transcripts (every
`transcript.jsonl` under it is one agent), or a local-backend store
(directories holding a `manifest.json` with `message_format`). `state.json`
next to a transcript is kept verbatim in `agent.source.extra.state_json`;
`payload-*.json` reflection payloads and other sidecars are not imported.

## Variants

Letta Code records **no producer version** anywhere in these files, so variants
are structural. The transcript is append-only and survives upgrades, so one
file can mix row eras; ids are taken per row.

| variant | Letta Code | row shape |
|---|---|---|
| `transcript-v0` | 0.18.3 – 0.21.x | no ids: `{kind, text, captured_at}` / `{kind:"tool_call", name, argsText, resultText, resultOk, captured_at}` |
| `transcript-v1` | 0.22.0 – 0.23.10 | + `source_line_id` on every row |
| `transcript` | ≥ 0.23.11 (0.34.x today) | + `source_message_id` on user/assistant/reasoning rows when the backend echoed one |
| `transcript-external` | 0.27.28 – 0.31.5 | `letta dream --from` rows: `source_message_id` on every kind, no line id |
| `local-backend` | ≥ 0.26.4 | pi session v3 entries (`manifest.message_format: "pi-session-entry-jsonl"`); message objects carry `id` and `metadata`; a message is re-appended as a new snapshot when it changes (latest wins) |

The writer (`appendTranscriptDeltaJsonl`) appends one batch per **completed**
turn (`end_turn`), `JSON.stringify` per row, every row of a batch sharing one
`captured_at` (flush time, not message time). Interrupted or failed turns are
not written. Row kinds are exactly `user`, `assistant`, `reasoning`, `error`,
`tool_call`; a tool row holds both the call and its result.

## Mapping (transcript)

Event ids: `source_line_id`, else `source_message_id`, else `@<byte offset>`;
part 1 of a tool row is `<id>#1`. Rows sharing a `source_message_id` (a
reasoning row and its assistant row, or `-split-N` assistant rows) share
`group`. `ts` = `captured_at`.

| row | events |
|---|---|
| `user` | `text` user (a SPEC §3.1.6 `<peer-message …>` wrapper becomes `peer_message`) |
| `assistant` | `text` assistant (likewise) |
| `reasoning` | `reasoning` {text} |
| `error` | `error` {kind: client, message} |
| `tool_call` | `tool_call` {call_id: `source_line_id` (it is the provider's tool call id) / `source_message_id` / `letta-code-tool-line-<n>`, name, input: parsed `argsText` (or `input_text` when it is not a JSON object, and then tool_type `custom`, SPEC §3.1.2), tool_type: `shell` for `Bash`}; then, if `resultText`/`resultOk` is present, `tool_result` {output: resultText, is_error: !resultOk}. An unfinished tool has no result. |
| other kinds, malformed lines | `native` (`row-native-only`, `malformed-line`) |

The local backend is imported with pi's mapping ([pi.md](pi.md)); a snapshot
that a later line of the same `message.id` replaces is kept `native` only
(`snapshot-superseded`). `manifest.json`, `conversation.json` and
`system-prompt.json` are kept verbatim in `agent.source.extra`.

## Export

Native rows found intact are written back unchanged (self trip byte-exact on
all 9 fixtures, the local-backend store included). Local-backend ledgers are
written back to their store; everything else is written as a current-shape
`transcript.jsonl` at `agent-<uuid>/conv-<uuid>/transcript.jsonl` (UUIDv5 of the
root and the ledger id), without `state.json` (Letta Code rebuilds it):

- `text` user → `user` row; assistant → `assistant` row; `reasoning` →
  `reasoning` row. Every user/assistant row gets a `source_message_id`
  (`message-<uuid of the group or event>`), because Letta's reflection only
  anchors on rows that have one; assistant rows of one message get
  `-split-<n>` line ids, as Letta's own splitter does.
- `tool_call` → `tool_call` row with `source_line_id` = call id, `argsText` =
  the input as JSON (or `input_text`), and `resultText`/`resultOk` from the
  matching `tool_result` (rows hold both, as Letta's do). A result with no
  call becomes a nameless tool row (`tool-result-orphan`).
- `error` → `error` row.
- `peer_message` → a `user` row (incoming) or `assistant` row (outgoing) whose
  text is the SPEC §3.1.6 wrapper, restored as a `peer_message` on import.
- No row exists for: media (`media-dropped`), compaction
  (`compaction-dropped`), settings, usage, title, subagent events, file
  changes, turns, app and native events (`*-dropped`). Sealed
  reasoning keeps its text only (`reasoning-sealed-dropped`). System/developer
  text becomes a user row (`system-text-as-user`). A subagent ledger becomes its
  own conversation (`subagent-separate-conversation`).

Exporting a foreign ledger **to the local backend** is not supported (it needs
agent and conversation records Letta Code would trust); the `local-backend`
variant only writes back ledgers that were imported from it.


## Branches (SPEC §2.3)

Letta Code transcripts are message lists read in order: no branch can be expressed. Decision: **not expressible** — the exporter renders the active
path, names `abandoned-branch-not-rendered`, and carries the abandoned events on
the next row. Done for the transcript eras (their writer) and the local-backend store (the shared pi session writer). The comparator now reads `after` (`<kind>.branch` costs what dropping the event costs, `<kind>.after` a wrong predecessor), so writing the abandoned branch as the conversation is no longer free: `protoledger/branch-rewind` via letta-code was 9 with the branch written inline (where the comparator could not see it) and is 17 with the active path rendered, its 4 abandoned events (13 facts) missing; written inline it would score 23 under the new comparator. With carry the trip still loses nothing (carried score 0).

## Carry

Yes. Letta Code parses rows with `JSON.parse` and ignores unknown keys, so each
rendered row carries `protoledger: {v, h, events}`; the first one also carries
the ledger's `agent`. A tool result's event rides with the row after its
call's row so its position survives. Events with no row ride on a neighbour,
or, when a ledger renders no row at all, on a row of kind `protoledger.carry`,
which Letta's reader and trajectory skip as an unknown kind.

## Verified

- Self trip byte-exact on all 9 fixtures. Six were written by Letta Code's own
  code run under Bun: the current writer, the v0.18.3 and v0.22.0 writers, the
  v0.27.28 external writer, and the `LocalBackend` store; two are trajectory's.
- Loadability: every letta-code export of the pi and omp fixtures (carry on
  and off) was fed to Letta Code's own `buildAutoReflectionPayload`
  (letta-code `69b7f82c`, Bun): all non-empty transcripts produced reflection
  payloads through `@letta-ai/trajectory` normalisation, none failed.
- pi/omp → letta-code → pi/omp with carry is byte-exact on every fixture, and
  letta-code → pi → letta-code with carry on every letta-code fixture.

Unverified: real captured user transcripts (none are published under a usable
license); local-backend message snapshots and compaction entries (handled,
not seen in generated data); resuming a conversation from an exported
transcript is not a Letta Code feature.

## Prefix stability (SPEC §9.3)

Transcripts are prefix-stable (`tests/prefix.rs`, 64 (file, k) pairs, 0 unstable). The era (`transcript-v0`, `-v1`, `transcript`, `transcript-external`) is decided by fields later rows may carry, so it is kept on the agent header only, and no native record carries it.
