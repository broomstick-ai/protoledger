# Live sync, tested against a simulated harness

The property under test: *a projected or attached session is followed live; what the
harness appends reaches the ledger without duplicates.* The suite asserts it
in `tests/sim_harness.rs`, an ordinary test target that plain `cargo test`
runs. A **simulator** writes into a temporary session the native records a
real harness writes, at the pace the real harness writes them. The real sync
engine follows that session with a `Follower` on its **default options**
(watcher on, poll 1 s, debounce 150 ms), and the test checks the ledger.

**Real-harness runs are out of scope for this repository**. No test
runs `claude`, `opencode` or any other harness, logs in, uses the network, or
reads a keychain, the real home directory, a harness's data directory
(`~/.claude`, `~/.codex`, `~/.local/share/opencode`), a real session or a
credential. Each test writes only under its own temporary directory and
asserts that it did (below). The earlier `tests/live` target ran a real
`claude -p` and `opencode run` turn. It linked the macOS login keychain into a
throwaway home to do so, and it has been removed.

## What the simulator writes, and where the shapes come from

| harness | session | shapes taken from (committed fixtures) | how it is written |
|---|---|---|---|
| Claude Code | `protoledger/minimal` projected (`sync::project`) into `<tmp>/claude/projects/-sim-work/` | `fixtures/claude-code/file-edit` (Claude Code 2.1.280, a real run): a prompt and its attachments, then three assistant messages, each split over several lines with one `message.id` (thinking, text, `tool_use`; the second message writes a second `tool_use` line after the first call's result), every line carrying the message's final `usage` and `stop_reason` | The session's lines are appended one at a time, each after the gap the fixture's own timestamps show (5 ms for a line with none), each one open-append-close (`appendFileSync`). The text line of the second message is flushed half written, with no newline, held for 1.3 s (past one poll and the debounce), then completed. |
| opencode 2.x | `ses_eb8d577daffemT7plHSFKatVSE` of `fixtures/opencode/v2-sqlite-2.0.31-generated`, imported, then projected into a fresh `<tmp>/opencode-data/opencode/opencode.db` (`v2-sqlite`) | the `user`, `assistant` and `idle` rows of `fixtures/opencode/_live-revert-2.0.32` (opencode 2.0.32) and the tool row of `v2-sqlite-2.0.31-generated` (opencode 2.0.31). The tool's `streaming` and `running` states follow `tests/opencode_sync.rs` | One connection held open for the whole turn, as opencode holds it. It runs in WAL mode with automatic checkpoints off, so every write stays in `opencode.db-wal` and `opencode.db` does not change (checked before and after). It writes the prompt row, then rewrites the assistant row in place (tool `streaming` → `running` → `completed`, a text growing, the message completing with its usage), then the `idle` row, touching `session_v2.time_updated` each time. The writes are 50–400 ms apart. |

The simulators never read anything from this machine but those fixtures.

## Each property, and the test that asserts it

`claude_code_simulated_turn_once_each_exact_within_2s_syncs_mid_line_no_divergence_no_reimport_for_a_1s_poll_after`
(Claude Code) and
`opencode_simulated_turn_in_wal_once_each_exact_within_2s_no_divergence_for_a_1s_poll_after`
(opencode) each assert the following. Once every event of the turn is in
the ledger, the follower keeps stepping for 1.3 s more (`QUIET`: one poll
period of 1 s, the longest period of the default options, and the 150 ms
debounce, and some), so that a sync after the last event is checked too.

| property | how it is asserted |
|---|---|
| **Every appended event reaches the ledger exactly once** | The turn's events are the events a fresh import of the location now makes that it did not make before. That import is computed by the importer directly, with no store and no sync involved. Each of those events is in the ledger once, and the ledger gained nothing else. For opencode, the one other event allowed is a `native` event carrying a newer version of the assistant row (SPEC §4.1). |
| **Native records are byte-exact** | Claude Code: every line the simulator appended is, byte for byte, the native record of exactly one event's part 0. opencode: for every row the turn wrote, the newest native record of it in the ledger is that row. Its `data` column matches byte for byte and every other column is equal. |
| **The last event is in the ledger within 2 s of the turn ending** | This is an explicit assertion. The test takes the time each event was first appended by the follower and subtracts the time the simulator's last write finished. The largest difference must be ≤ 2 s. The follower keeps stepping for up to 10 s after the end, so a late event shows up as late rather than as missing. The test prints what made the sync that appended the last event run (the watcher or the poll). |
| **A sync mid-message never triggers a divergence or a re-import** | Some sync appended events before the turn ended, so the location was read mid-turn. No `protoledger/diverged` event was appended, and no sync up to `QUIET` past the last event found the location diverged; this is checked first, before the test reads the location again. For Claude Code, at least one sync was made while a line was half written. What is counted there is sync *calls*: the sync engine does not read a file whose final line has no newline (`torn` in `src/sync`), so those syncs are deferred and read nothing. The test also prints how often the binding's cursor moved in that window (a sync that read the file moves it): 0 in each of six runs on macOS on 2026-10-20, against 1 with the torn-line check bypassed (below). A partial line that a follow does read is held by the format's follow itself; that path is reached only with the torn-line check bypassed, and `tests/follow.rs` asserts it directly (a partial line yields no events). After the follower's first read, every sync that appended read on from where the last one stopped (`Synced::followed`): there was no whole re-import, and the follower still follows the file at the end. opencode's store is a database, so every sync of it is a whole read through the in-place hook, and only "no divergence" applies to it. |
| **Usage is booked as Claude Code writes it** (Claude Code) | There is one `usage` event per message, `<message.id>#usage`. Its output count is the one the message's last line states. |
| **The opencode write was never checkpointed** (opencode) | The WAL grew, and `opencode.db`'s bytes and modification time are unchanged after the follower synced. What the follower read therefore came from the WAL. |
| **Nothing outside the temporary directory** (both) | The test lists every path the run resolved or wrote: the store root, `index.db`, every ledger, every binding's location with its `-wal` and sidecar path, and the files the simulator wrote. Each must lie under the test's temporary root, and none may lie under the real `$HOME` outside it. Every fixture it read must be under the repository's `fixtures/`. The check prints how many paths of each kind it checked. `$HOME` is only compared against: nothing under it is opened or even resolved. |

`claude_code_rewind_fixture_imports_as_an_after_branch` and
`opencode_staged_revert_fixture_imports_as_an_after_branch` are also in
`tests/sim_harness.rs`. They moved there unchanged from the removed target.
They check that the importers read the rewind Claude Code wrote
(`fixtures/claude-code/_live-rewind-2.1.280`) and the revert opencode staged
(`fixtures/opencode/_live-revert-2.0.32`) as `after` branches (SPEC §2.3).
Those two fixtures were captured once from real runs, and their `SOURCE.md`
files say how. They are repository data, and nothing regenerates them.

## Forcing a guard red

`PL_SIM_FORCE` makes one guard fail. It is never set in CI:

| `PL_SIM_FORCE=` | what changes | what fails |
|---|---|---|
| `no-follower` | nothing follows the binding | "once each": the first event is in the ledger 0 times (both tests) |
| `late-follower` | the follower stops for 2.5 s when the turn ends, then carries on | the 2 s bound: the last event arrives about 2.5 s after the end, though every event does arrive once (both tests) |
| `old-usage` | the Claude Code simulator writes a partial output count and no stop reason on every line of a message but its last, as Claude Code did before 2.1.2xx | the usage the ledger books: the count of a line that does not settle the message, not the final one |
| `home-path` | a path under the real `$HOME` (computed only, never written or read) is added to the isolation check's population | the isolation check (both tests) |
| `late-divergence` | once every event is in the ledger, the location is changed so that the next sync finds it diverged: the Claude Code file loses its last line, the opencode prompt row is deleted (through the held connection, so only the poll sees it) | "no divergence" (both tests), which sees it only because the follower steps `QUIET` past the last event |
| `short-hold` | the Claude Code simulator completes the split line at once | "at least one sync while a line was half written" (0 syncs) |

```sh
PL_SIM_FORCE=late-follower cargo test --test sim_harness -- --nocapture
```

Two guards check product behaviour, so the code is changed to force them, and
the change is never committed:

- **A partial line read as whole.** Make `torn` in `src/sync/mod.rs` return
  `None` *and* make the Claude Code follow (`feed`, in
  `src/formats/claude_code/import/follow.rs`) parse a held partial line as a
  whole one. The split line's event is then never appended once. Bypassing
  `torn` alone stays green, because the follow holds a partial line by itself;
  the cursor count printed for the half-written window then goes from 0 to 1
  (a real read of the half-written file).
- **The opencode WAL not observed.** Leave `<file>-wal` out of the location's
  files and out of the watcher's match in `src/sync`. Nothing is synced during
  the turn, because the database file itself never changes.

## Observations kept from the real runs

These were measured on 2026-10-19 with Claude Code 2.1.280 and opencode
2.0.32, before the real-harness tests were removed. The simulators are built
to reproduce them:

- Claude Code writes its lines as the turn goes. The follower appended them
  in syncs that landed before `claude` exited, and the last one arrived
  85–149 ms before the exit.
- opencode writes in WAL mode and holds the database open for its whole run.
  **On macOS the watcher does not see a write to `opencode.db-wal` while the
  writer keeps the file open**: FSEvents reports a content change when the
  writer closes the file. This was measured while writing the WAL tests in
  `tests/opencode_sync.rs`, with a held connection and the watcher only (60 s
  poll): there was no event for the WAL and no sync in 5 s. So during an
  opencode turn the **poll** (1 s by default) is what reads the WAL, and the
  stamp covers `-wal` for that reason. A writer that closes the file (a second
  process) is seen by the watcher (`wal_write_by_a_closing_connection…`). In
  the simulated turn the last event arrives about 0.9 s after the end, and
  the test prints that it is the **poll** that appended it, not the watcher:
  measured on macOS over five runs on 2026-10-20, +885 to +916 ms, each
  `Poll-triggered`. So the margin under the 2 s bound is what is left of
  the bound after one poll period (about 1 s), which a slower CI runner
  eats into; the bound stays 2 s.

## Not covered

- Real-harness runs, by design (above). A harness version that writes a
  different shape is covered when its fixture is added (CONTRIBUTING.md) and
  a simulator is pointed at it.
- `sync::project` cannot project into a 1.x opencode storage tree
  (`find_location` needs a single file). `tests/opencode_sync.rs` attaches
  and drives the 1.x stores instead.
- Linux behaviour is asserted only by CI's Linux runners. inotify reports
  each write, so the watcher would see the WAL there too.
