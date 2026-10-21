# _live-revert-2.0.32

A real opencode **staged revert** (SPEC §2.3): two turns of one session, then
`session.revert` staged at the second turn's prompt, with the reverted
messages still in the database.

- Turn 1: `opencode run --standalone -m opencode/glm-5.3-flash-free "reply
  with the word ok"` → "ok".
- Turn 2: `opencode run --standalone -s <session> -m … "reply with the word
  two"` → "two".
- `opencode api --standalone POST /api/session/<session>/revert/stage -d
  '{"messageID":"msg_153dcf904001iPgLeMHByB3FhG"}'` (operation
  `session.revert.stage`) → `session_v2.revert` =
  `{"messageID":"msg_153dcf904001iPgLeMHByB3FhG","files":[]}`.

`opencode run` has no revert; the server API is how it was staged.

**Not a conformance case** (the leading `_`, as `tests/fixtures.rs` reads
it), for the same reason as `claude-code/_live-rewind-2.1.280`. It was
produced by the live scripts of `tests/live`, which have since been
removed; no test runs opencode now, and nothing regenerates this
fixture. The current test is the simulated harness, `tests/sim_harness` (see
`docs/LIVE-TESTS.md`): `opencode_staged_revert_fixture_imports_as_an_after_branch`
there checks that the opencode importer reads it as an `after` branch
continuing from turn 1's last record, with turn 2 off the active path, and
the simulated opencode turn takes its `user`, `assistant` and `idle` row
shapes from it.

## Origin

Generated once, by the since-removed live scripts, running opencode **2.0.32** (Homebrew, darwin-arm64) on
2026-10-19 in a throwaway directory `/private/tmp/pl-live-r3s4-ocrev`, with
`XDG_DATA_HOME`, `XDG_CONFIG_HOME`, `XDG_CACHE_HOME` and `XDG_STATE_HOME`
under it and `--standalone` (a private server on that data directory, not the
background service), to test live sync. Model: the free
`opencode/glm-5.3-flash-free`, anonymous.

## License

Generated for protoledger; MIT OR Apache-2.0, like the crate.

## Sanitisation

`tests/live/sanitize_opencode.py`, removed with the rest of `tests/live`
(run as `sanitize_opencode.py <db> <out>
/private/tmp/pl-live-r3s4-ocrev/work /private/tmp/pl-live-r3s4-ocrev
revert`): the database copied with SQLite's backup API (WAL included) and set
to `journal_mode=DELETE`; the working directory rewritten to
`/home/user/work/revert` and the throwaway root to `/home/user`, a macOS
per-user temporary directory to `/tmp`, in every text value of every table;
every `instruction_blob` value (the system instructions opencode assembled,
which included a listing of skills it found in the user's home directory)
replaced by `"[instruction elided from this fixture]"`; VACUUMed. Ids,
timestamps, token counts and the project id are unchanged. The prompts were
neutral and written for this fixture.
