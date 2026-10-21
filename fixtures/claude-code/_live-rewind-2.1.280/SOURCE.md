# _live-rewind-2.1.280

A real Claude Code rewind (SPEC §2.3): three turns of one session, the third
of which Claude Code itself wrote as a branch back to the first.

- Turn A: "reply with the word ok" → "ok".
- Turn B (`--resume`): "reply with the word two" → "two".
- Turn C (`--resume <id> --resume-session-at <turn A's answer uuid>`): "reply
  with the word three" → "three". Its user line's `parentUuid` is turn A's
  answer (`0547eff8-…`), so turn B is an abandoned branch, as after an
  interactive `/rewind` to that message. `--resume-session-at` is the flag
  behind the Agent SDK's `resumeSessionAt`; it is not in `claude --help`.

`/rewind` itself is interactive (a menu), so `claude -p` cannot run it; the
branch here is the same shape — a line appended to the same file whose
`parentUuid` goes back past lines of its own conversation — produced the
non-interactive way.

**Not a conformance case** (the leading `_`, as `tests/fixtures.rs` reads it):
adding a case adds a report and a MATRIX.md row, and those are shared with
another step at the time of capture. It was produced by the live
scripts of `tests/live`, which have since been removed; no
test runs Claude Code now, and nothing regenerates this fixture. The current
test is the simulated harness, `tests/sim_harness` (see
`docs/LIVE-TESTS.md`): `claude_code_rewind_fixture_imports_as_an_after_branch`
there checks that the claude-code importer reads it as an `after` branch and
that its self trip is byte-exact; `tests/prefix.rs` walks
every `fixtures/claude-code/*/native`, so it also checks the importer is
prefix-stable on it. Promoting it to a case is renaming the directory and
regenerating the reports.

## Origin

Generated once, by the since-removed live scripts, running Claude Code 2.1.280 (`claude -p --safe-mode --model
haiku --output-format json`) on 2026-10-19 in a throwaway directory
`/private/tmp/pl-live-r3s4-ccprobe/work`, with `HOME` set to a throwaway
directory beside it (so the session was written under it, not under the
user's `~/.claude`), to test live sync.

## License

Generated for protoledger; MIT OR Apache-2.0, like the crate.

## Sanitisation

`tests/live/sanitize_claude_code.py`, removed with the rest of `tests/live`
(run as
`sanitize_claude_code.py <session> <out> /private/tmp/pl-live-r3s4-ccprobe/work
/private/tmp/pl-live-r3s4-ccprobe/home rewind`):

- rewrote the working directory to `/home/user/work/rewind` (and the project
  slug to `-home-user-work-rewind`), the throwaway home to `/home/user`, and
  the account e-mail address to `user@example.com`;
- replaced the system prompt, tool descriptions and tool schemas of
  `prompt_snapshot` attachments, and the text of the `skill_listing`
  attachment, with short `[... elided from this fixture]` placeholders,
  keeping the record shape. These lines were re-serialised with compact JSON
  after checking the original line re-serialised byte for byte.

Nothing else was changed. The prompts were neutral and written for this
fixture.
