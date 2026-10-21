# json-real-anonymized-slice

**Origin:** `memory-tests/large-chat-session.json` in
https://github.com/google-gemini/gemini-cli at commit
`ed96faedfd5571f055c44f2c9509d62321d1138d` (file added in a74bb603c,
"feat(core): add large memory regression test (#25059)", 2026-04-10). It is a
real Gemini CLI session (whole-file JSON, so a 0.3x producer, April 2026) that
Google anonymised before publishing: ids, hashes and e-mail addresses are
replaced by `anonymous_*` placeholders.

**License:** Apache-2.0 (the gemini-cli repository).

**Sanitisation / changes:** the original is 56 MB with 1472 messages. This
case keeps 24 of them, in order: indices 0-6, 31-39, 320-324 and 1374-1376
(the opening task, a completed stage and the follow-up questions, a cancelled
shell call and the "Request cancelled." info notice, and a memory warning).
Everything between them is elided. The slice was written with Node's
`JSON.stringify(doc, null, 2)` (no trailing newline), which is how Gemini CLI
writes this file; the published original had been partly reflowed by a
formatter (some short arrays on one line), so its bytes are not producer
bytes. No content inside a message was changed. Checked: no `/Users/`,
`/home/` or real e-mail addresses remain.

**What it exercises:** real tool names and argument shapes (`update_topic`,
`list_directory`, `read_file`, `replace`, `write_file`, `grep_search`,
`run_shell_command`), real `resultDisplay` objects (FileDiff with diffStat,
list results), `displayContent`, thoughts with subjects, tokens, `cancelled`
and `error` statuses, `info` and `warning` messages.
