# checkpoint-v0.64

**Origin:** generated on 2026-10-16 by running Gemini CLI's own recorder,
`@google/gemini-cli-core@0.64.0` (npm, Apache-2.0, from google-gemini/gemini-cli
tag `v0.64.0`), driven by `../_generators/save-checkpoint.mjs`:

```sh
npm install --ignore-scripts @google/gemini-cli-core@0.64.0   # in a scratch dir <core>
node _generators/save-checkpoint.mjs <core> <out> object 'demo tag'
```

The script calls the real `Logger.saveCheckpoint` with the same call sequence
`GeminiChat` uses (it feature-detects APIs missing in older versions). No model
was called: prompts, model replies, tool outputs and token counts are written
into the script, so the bytes are what that version of Gemini CLI writes for
them, but the conversation is scripted. Ids and timestamps are the ones the
recorder minted on that run.

**What it exercises:** a `/chat save demo tag` checkpoint in the 0.16.0+ `{history, authType}` form: Gemini API Content history with the session-context preamble, a thought part with a thoughtSignature, two parallel functionCalls answered by two functionResponses (one an error), and an inline image. The file name is the percent-encoded tag.

**License:** the recorded data is authored for this repository (MIT OR
Apache-2.0); the recorder that wrote it is Apache-2.0.

**Sanitisation:** none needed: neutral prompts, project root
`/home/user/project` (so `projectHash` is sha256 of that path), no
credentials, no personal data.
