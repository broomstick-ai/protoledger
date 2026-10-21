# jsonl-v0.50-history-sets

**Origin:** generated on 2026-10-16 by running Gemini CLI's own recorder,
`@google/gemini-cli-core@0.50.0` (npm, Apache-2.0, from google-gemini/gemini-cli
tag `v0.50.0`), driven by `../_generators/record-session.mjs`:

```sh
npm install --ignore-scripts @google/gemini-cli-core@0.50.0   # in a scratch dir <core>
node _generators/record-session.mjs <core> <out> 
```

The script calls the real `ChatRecordingService` with the same call sequence
`GeminiChat` uses (it feature-detects APIs missing in older versions). No model
was called: prompts, model replies, tool outputs and token counts are written
into the script, so the bytes are what that version of Gemini CLI writes for
them, but the conversation is scripted. Ids and timestamps are the ones the
recorder minted on that run.

**What it exercises:** the 0.44-0.63 JSONL shape, where every history sync appends the whole message list as `{"$set":{"messages":[...]}}`, plus re-appended messages, a subagent session and a `$rewindTo`.

**License:** the recorded data is authored for this repository (MIT OR
Apache-2.0); the recorder that wrote it is Apache-2.0.

**Sanitisation:** none needed: neutral prompts, project root
`/home/user/project` (so `projectHash` is sha256 of that path), no
credentials, no personal data.
