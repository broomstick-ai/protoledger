# omp (oh-my-pi)

oh-my-pi (`can1357/oh-my-pi`, npm `@oh-my-pi/pi-coding-agent`, runs on Bun) is
a fork of [pi](pi.md) and keeps pi's session tree. The adapter is the shared
`src/formats/pi_session.rs` with an omp dialect; everything in the pi doc about
branches, ids, native records and carry applies. This page lists what differs.
omp's own reference is `docs/session.md` in its repo.

## Storage

```
~/.omp/agent/sessions/<cwd bucket>/<timestamp>_<session id>.jsonl
~/.omp/agent/sessions/<cwd bucket>/<timestamp>_<session id>/<AgentId>.jsonl   subagents
~/.omp/agent/sessions/<cwd bucket>/<timestamp>_<session id>/<AgentId>/<Child>.jsonl
~/.omp/agent/blobs/<sha256>                                                  externalised images
```

The input may be a session file or a directory. In a directory, a file
`D/<name>.jsonl` next to `D.jsonl` is a subagent of `D.jsonl`; advisor
transcripts (`__advisor*.jsonl`) and dot files are skipped. Other artifact
files in the session directory are not imported.

## Variants

| variant | omp versions | shape |
|---|---|---|
| `header-first` | < 16.2.3 | pi-shaped: header first |
| `title-slot` | ≥ 16.2.3 (18.x today) | line 1 is a fixed-width **256-byte** `{"type":"title","v":1,"title","source"?,"updatedAt","pad"}` record that omp overwrites in place on rename; then the header |

Both use session `version: 3` and pi's v1/v2 migrations. Every entry written
by omp ≥ 18.11.0 carries `runtime: {version, commit?, branch?}`: the adapter
puts `runtime.version` in each event's `native.version` and the first one in
`agent.source.version`. Detection prefers omp over pi when it sees the title
slot, `runtime`, omp-only entry types, `model_change.model`, or a `.omp/` path.

## Mapping (differences from pi)

| omp record | events |
|---|---|
| title slot | `title` (or `native` when empty) |
| header `title`, `additionalDirectories` | `title`; `settings.additional_directories` |
| `model_change` {model: "provider/id", role} | `settings` {provider, model} when role is default; other roles (smol, slow, …) → `native` |
| `thinking_level_change` {thinkingLevel: null} | `settings` {effort: "off"} |
| message `developer` | `text` role developer |
| `title_change` | `title` |
| `model_usage` | `usage` {purpose}; its `activeLeafId` decides the leaf (omp's `resolveSessionLeafId`) |
| `reset_boundary` (`/clear`) | `compaction` {phase: boundary, trigger: manual, reason: reset} |
| `session_init` | `settings` {system_prompt, tools, agent} |
| `task` tool result | `tool_result`, then one `subagent` {phase: end, agent: child ledger id, call_id, agent_type, description, outcome, result} per `details.results[]` |
| images `blob:sha256:…` | `media` with `url` = the blob ref (loss `media-blob-ref`) |
| `mode_change`, `service_tier_change`, `ttsr_injection`, `credential_pin`, `custom`, `label`, `fileMention`, `pythonExecution` | `native` |

**Subagents.** `D/<AgentId>.jsonl` becomes its own ledger with
`agent.parent = {agent: <ledger holding the task call>, event: <task tool_call
event>, relation: subagent}`; the link is the `task` result's
`details.results[].id`, which is the subagent file's stem.

## Export

As pi, plus: a title slot is written first (exactly 256 bytes, title
truncated by code points as omp does) unless `--variant header-first` or a
`target_version` below 16.2.3; model changes use `model: "provider/id"`;
titles become `title_change` {source: user}; system/developer text becomes a
`developer` message; foreign system prompts are dropped (`system-prompt-dropped`:
omp rebuilds its prompt); unattached usage becomes `model_usage`. A subagent
ledger is written as `<parent stem>/<agent type>-<id hash>.jsonl`, so omp's
`collectSubSessions` finds it. Same loss codes as pi otherwise.


## Branches (SPEC §2.3)

As for [pi](pi.md#branches): omp keeps pi's session tree, and the importer turns every branch off the active path into its own fork ledger. Decision: **not expressible** — the exporter renders the active
path, names `abandoned-branch-not-rendered`, and carries the abandoned events on
the next record. Done for every variant (the shared pi session writer). The comparator now reads `after` (`<kind>.branch` costs what dropping the event costs, `<kind>.after` a wrong predecessor), so writing the abandoned branch as the conversation is no longer free: `protoledger/branch-rewind` via omp was 8 with the branch written inline (where the comparator could not see it) and is 17 with the active path rendered, its 4 abandoned events (13 facts) missing; written inline it would score 22 under the new comparator. With carry the trip still loses nothing (carried score 0).

## Carry

Yes, on every rendered entry and the header, as for pi. **Not** on the title
slot, which must stay 256 bytes; on re-import with `restore_carried`, a title
slot in a file whose header carries a protoledger agent is treated as derived
and skipped.

## Verified

- Self trip byte-exact on all 4 fixtures, including the directory fixture with
  a subagent generated by omp 18.11.3's own `SessionManager` under Bun.
- Exports load in the real harness: every omp export of the pi fixtures, and
  every fixture, opened with omp 18.11.3's `SessionManager.open` +
  `buildSessionContext` (and `collectSubSessions` for subagents) under Bun
  without errors.
- omp → pi → omp with carry is byte-exact on every fixture.

Unverified: omp < 16 files (no samples beyond trajectory's synthetic ones);
blob-store images are not resolved; artifact files beside a session are
ignored; Redis/SQL session storage backends are not read.

## Prefix stability (SPEC §9.3)

Not prefix-stable where a session holds a branch, as for [pi](pi.md#prefix-stability-spec-93): the active path is read from the latest leaf. `tests/prefix.rs`: 9 of 92 (file, k) pairs differ, all in the three fixtures that hold a branch (`cleanup`, `subagent-branches`, `tool-calls`); `migrated-title-slot` holds none and is stable. The count is pinned there.
