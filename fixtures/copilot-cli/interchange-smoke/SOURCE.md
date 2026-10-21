# interchange-smoke

**Origin:** hand-written for this repository on 2026-10-16 in the "semantic
JSONL interchange v1" that `copilot sessions import` (>= 1.0.85) reads.
Copilot CLI 1.0.98 accepted it (its output is `../dir-cli-1.0.98-imported`).
The `note.txt` attachment it names is not part of this case.

**License:** MIT OR Apache-2.0 (this repository).

**What it exercises:** the `session` header (title, source, workspace with
repository and branch), user / assistant `message` records with a text and
an attachment part, `tool-call` and `tool-result` (one failed with
`error {message, code}`).
