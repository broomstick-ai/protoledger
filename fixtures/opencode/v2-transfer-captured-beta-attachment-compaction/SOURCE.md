# v2-transfer-captured-beta-attachment-compaction

- **Messages:** `internal/parser/testdata/opencode_v2/attachment_compaction.json` of
  [kenn-io/agentsview](https://github.com/kenn-io/agentsview) at commit
  `905a5fe2ee7c01fb55f620d9ce4e4728aed177ae` (MIT) — "captured from beta 19381's
  prompt and compact APIs": a user message with no text and an inline
  `text/plain` attachment, two assistant steps (reasoning, a `read` tool), and a
  completed **manual compaction**.
- **Container: synthetic.** The rows were turned into an
  `opencode session export` document (`SessionTransfer.Data`,
  `packages/schema/src/session-transfer.ts`) by `mkfix.py`: each message is
  `{id, …data, type}` with keys in the encode order of
  `packages/schema/src/session-message.ts`; `info` was written from
  `packages/schema/src/session.ts` (token totals summed from the rows; title
  invented); layout is `JSON.stringify(data, null, 2) + EOL` as
  `packages/cli/src/commands/handlers/session/export.ts` prints it.
- **Exercises:** media import (`files[]`), compaction summary, transfer detection.
- **Sanitised:** by agentsview.
