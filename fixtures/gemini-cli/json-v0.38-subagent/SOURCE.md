# json-v0.38-subagent

**Origin:** generated on 2026-10-16 by running Gemini CLI's own recorder,
`@google/gemini-cli-core@0.38.0` (npm, Apache-2.0, from google-gemini/gemini-cli
tag `v0.38.0`), driven by `../_generators/record-session.mjs`:

```sh
npm install --ignore-scripts @google/gemini-cli-core@0.38.0   # in a scratch dir <core>
node _generators/record-session.mjs <core> <out> 
```

The script calls the real `ChatRecordingService` with the same call sequence
`GeminiChat` uses (it feature-detects APIs missing in older versions). No model
was called: prompts, model replies, tool outputs and token counts are written
into the script, so the bytes are what that version of Gemini CLI writes for
them, but the conversation is scripted. Ids and timestamps are the ones the
recorder minted on that run.

**What it exercises:** the last whole-file JSON version: one `JSON.stringify(record, null, 2)` document per session (no trailing newline), `kind`/`summary`/`directories` after `messages`, thoughts and tokens on gemini messages, tool calls with inline results, FileDiff and SubagentProgress `resultDisplay`, info/error notices, and a subagent session file. (0.38 has no rewind record; the scripted rewind is a no-op there.)

**License:** the recorded data is authored for this repository (MIT OR
Apache-2.0); the recorder that wrote it is Apache-2.0.

**Sanitisation:** none needed: neutral prompts, project root
`/home/user/project` (so `projectHash` is sha256 of that path), no
credentials, no personal data.
