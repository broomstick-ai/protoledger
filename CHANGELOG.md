# Changelog

All notable changes to protoledger are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate follows
[Semantic Versioning](https://semver.org/) (before 1.0, a minor version may
break the API).

## 0.1.0 — 2026-10-25

The first release. What you get:

- **A format.** One append-only JSONL file per agent, specified in SPEC.md:
  every event has a harness-neutral `kind` and `body` beside the harness's
  own record, so any exporter can render it and exporting back to its own
  harness is byte-exact. Subagents and forks are their own files; branches
  (a rewind, an edited message) are recorded, not lost.
- **17 formats, with their versions.** Import and export for Claude Code,
  Codex, opencode, Gemini CLI, Cursor, Copilot CLI, Droid, Hermes,
  Deep Agents, OpenClaw, OpenHands, pi, omp, Letta Code, ATIF, Slack and the
  ledger itself, each detecting the shape its producer version wrote
  (README.md, docs/formats/). Every loss is named and counted.
- **Carry.** A trip Claude Code → Codex → Claude Code (or through opencode)
  comes back exact, even where the middle harness cannot express everything.
- **A store.** A directory of ledgers with a rebuildable SQLite index,
  and garbage collection by an explicit policy. `protoledger cat <store>
  <agent>` writes an agent's ledger to stdout: the decompressed ledger, byte
  for byte.
- **Redaction.** Redact events of an agent (`protoledger store redact`),
  or withdraw that (`store unredact`): the ledger keeps every event
  unchanged and records the redaction as a new fact; every projection and
  export leaves the redacted events out (SPEC §11.1).
- **Sync.** Project a thread into a harness, or attach to a harness's own
  session, and what the harness appends keeps arriving in the ledger.
- **Re-projection.** `protoledger reproject <store> <binding>` rewrites a
  projection's file from the ledger minus its redacted events, so the
  harness resumes from a clean file while the ledger keeps everything. It
  records what it wrote as a new fact, so a later sync never reports
  protoledger's own rewrite as divergence. Run it only between harness runs,
  never while a harness that may write the file is running (SPEC §11.2).
- **Message delivery.** Record a message an application queued for an agent,
  and what became of it, as appended facts: queued, delivered, cancelled (an
  attempt that did not send it; still owed) or withdrawn. What should still
  be sent and what was sent are derived from those facts; nothing is
  rewritten (SPEC §11.3). Library only.
- **Serve.** The store over HTTP and a WebSocket, loopback by default.
- **Compression.** A store can keep its ledgers compressed (SPEC §10):
  `protoledger store config <store> --compression on` migrates every ledger
  to sealed zstd blocks plus a plain hot tail, and `off` migrates them back,
  each checked byte for byte before its old form is deleted;
  `--seal-bytes <n>` sets when a writer seals. Every read returns the
  decompressed ledger byte for byte, at the same offsets. No dictionary is
  used unless you configure one.
- **Dictionaries and recompression.** `store dict add <store> <file>` adds a
  zstd dictionary you pass, `store dict train <store> [--size <bytes>]` trains
  one from the store's own ledgers, `store config <store> --dictionary
  <id>|none` sets the one new blocks are sealed with, and `store recompress
  <store> [--dictionary <id>|none] [agents…]` rewrites ledgers with it, each
  new block file read back and checked byte for byte before it replaces the
  old one. `store dict ls` shows which ledgers and blocks each dictionary
  encodes (from the index, which `store reindex` rebuilds); `store dict rm`
  refuses while a dictionary is the default or any block uses it (SPEC §10.7,
  §10.9).
- **Damaged ledgers.** A store that finds a damaged ledger (a compressed one
  whose blocks or hot tail disagree, or a plain one that does not parse)
  still opens: the index skips that ledger, `Store::damaged` and every CLI
  command that reads the store name it (`store ls` marks its row), and every
  operation on it fails naming it until it is repaired by hand. Nothing in
  it is changed: `store gc` never removes it (nor its parent, when the index or
  the ledger's header names one) and lists it as
  `damaged` in its plan, a subscription to every agent (and `serve`'s feed)
  leaves it out while one naming it is refused, and sync defers its
  bindings.
- **Every mode tested.** The conformance, sync (simulated harness), serve,
  garbage-collection and reindex tests run in a plain store and in a
  compressed one with no dictionary, with a passed dictionary, and after a
  recompression with a trained one.
- The store's test hooks (`protoledger::store::testing`) are behind a
  non-default `test-hooks` feature; without it they compile to nothing.

### Format

- SPEC.md 0.1: one append-only JSONL ledger per agent; neutral `kind`/`body`
  on every event beside the harness's own record, kept as its exact bytes
  (`native.record`, SPEC §4); stable event ids across imports and round trips;
  branches (§2.3); named, counted losses on every import and export.
- Carry (§6): exporters stow protoledger events in a field the target
  tolerates, so a trip through Claude Code, Codex or opencode comes back exact.

### Formats

- Import and export for `claude-code`, `codex`, `opencode`, `gemini-cli`,
  `cursor`, `copilot-cli`, `droid`, `hermes`, `deepagents`, `openclaw`,
  `openhands`, `pi`, `omp`, `letta-code`, `atif`, `slack` and `protoledger`,
  each with its producer-version variants (README.md, docs/formats/).
- Cursor IDE (`state.vscdb`): every row of a thread comes back with its
  key, SQLite storage class and bytes; the rest of the file, which is the
  IDE's own state, is counted as skipped by name (`unlinked-row-skipped`,
  `ide-state-skipped`), never dropped silently.
- SQLite stores (Hermes, OpenClaw, Cursor, opencode, Deep Agents): the
  file's header values an application reads back (`PRAGMA application_id`,
  `user_version`, and `encoding` when not UTF-8) are kept in the ledger and
  written back on export, so a self trip keeps them; several files exported
  as one with different values keep the values of the ledger whose
  id sorts first, named
  `sqlite-header-conflict`; for Hermes, Deep Agents, Cursor and opencode
  every ledger of a file carries them, so a subset of a file's sessions
  writes them too, and store-level data a subset cannot rebuild is named
  (`sqlite-store-left-out`, Hermes and Deep Agents); OpenClaw keeps them in
  the store captured on its first session, and a store whose first session
  is changed or not exported is not rebuilt and is named, header values
  included (`sqlite-store-not-rebuilt`).
- opencode (`opencode.db`): every row of every table is kept, so the
  database re-exports row for row; the rows that belong to no session
  (projects, worktrees, instruction blobs, ...) go in one store ledger per
  database, which every session ledger is `member-of` and whose id stays
  the same as sessions are added or removed.
- A store container (a ledger of only native or app records that another
  ledger names `member-of`, such as opencode's store ledger or a Slack
  export's workspace ledger) is left out of an export without carry to a
  format other than its own and `protoledger`, counted as
  `container-not-rendered`, instead of being rendered as an empty
  conversation (SPEC §2.3).
- Sync of an attached `opencode.db`: a store row or todo row opencode
  deletes is appended as a tombstone, a newer version of that row (SPEC
  §4.1), and the binding keeps following; exports leave the row out.
- Sync of an attached `opencode.db`: a store row or todo row opencode
  rewrites in place (a todo going `pending` → `completed`) is appended as a
  newer version of that row (SPEC §4.1); exports write the newest version.
- A conformance suite over 191 fixture cases (self trips byte-exact unless
  `expect.json` says why not, cross trips with and without carry), ratcheted
  against committed reports; a mutation harness in which no importer panics.

### Library and CLI

- `protoledger::formats` (import, export, detect), `roundtrip`, `Appender`
  (locked, fsynced appends), `tail` / `read_at`.
- The `protoledger` binary: `formats`, `detect`, `import`, `export`,
  `convert`, `validate`, `roundtrip`; with `store`, `store
  init|ls|show|reindex|import|redact|unredact|gc`; with `sync`, `project`,
  `attach`, `unbind`, `reproject`, `watch`; with `serve`, `serve`.
- `Store` (feature `store`): a directory of ledgers plus a rebuildable SQLite
  index (SPEC §9.1); garbage collection by explicit policy (§9.5);
  `Store::redact` / `Store::unredact`, which append a redaction or
  unredaction fact and refuse, appending nothing, an event id that is not in
  the ledger (§11.1).
- Sync (feature `sync`): bindings that follow a harness's own files and append
  what it writes (§9.2, §9.3).
- `sync::reproject(store, binding)` (feature `sync`): rewrites a Claude Code
  or Codex projection's file from the projected view (the ledger minus
  redacted events and protoledger's own facts) and appends a
  `protoledger/projected` fact, against which sync measures the file
  (§11.2). It refuses a binding that is not an active projection, or whose
  location diverged. Run it only between harness runs: a harness still
  writing the file would lose what it appends after the rewrite.
- `protoledger::delivery` (§11.3): `message`, `delivered`, `cancelled` and
  `withdrawn` append the four delivery facts through an `Appender`, and
  `Store::message` / `delivered` / `cancelled` / `withdrawn` through a store.
  Each is checked against the ledger and refused, appending nothing
  (`DeliveryError`), for a duplicate message id, an empty or repeated list,
  an unknown message, or a message already delivered or withdrawn. A
  cancelled message is still owed; only `withdrawn` retracts one.
  `delivery::state(&ledger)` (or `Store::delivery`) derives each message's
  state and history, `should_send()` (queued − delivered − withdrawn) and
  `sent()`, ignoring any fact a writer would have refused. Library only:
  there is no CLI command for it.
- `serve` (feature `serve`): the store over HTTP and a WebSocket, loopback
  only unless `--allow-remote` (docs/SERVE.md).
- `serve::resolve_listen(listen, allow_remote)` takes `allow_remote` (new
  public parameter, with `serve::pick_listen`): under `--allow-remote` a host
  name listens on the first address it resolves to, loopback or not; without
  it, on its first loopback address, falling back to its first address when
  none is loopback (which `check_listen` then refuses).

### Features

- `default = ["cli", "sqlite", "zstd", "store", "sync", "serve"]`.
- `cli` builds the binary and is the only feature that pulls in clap: a
  library dependent with `default-features = false` does not build it.
- `serve` is the only feature that pulls in tokio and axum.
- serde_json is used with `raw_value` only: protoledger never turns on
  `preserve_order` or `arbitrary_precision` for its dependents.

### Hardening

- Every importer is run over byte-level mutations of every fixture; none
  panics.
- `cargo clippy -D warnings` (every target) and
  `cargo doc` with `-D warnings` are clean in the default, all-features and
  no-default feature sets; CI (`.github/workflows/ci.yml`) runs format,
  clippy and the whole suite (plain `cargo test`) on Linux and macOS. The
  suite is self-contained: live sync is tested against a simulated
  harness (`tests/sim_harness.rs`, docs/LIVE-TESTS.md), and no test runs a
  real harness or reads a keychain, the real home directory or a credential.
