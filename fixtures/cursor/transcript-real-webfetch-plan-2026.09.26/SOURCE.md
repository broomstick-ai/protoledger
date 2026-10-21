# transcript-real-webfetch-plan-2026.09.26

**Origin:** `fixtures/cursor/2026.09.26-dd393fe/transcript.jsonl` in https://github.com/civitaspo/herdr-infobox at commit `85bbeeb0be568f0b957d0c766f112d3dd9a45768`, copied byte for byte.

Real `~/.cursor/projects/<slug>/agent-transcripts/<id>.jsonl` projection from the same two runs as `stream-real-webfetch-plan-2026.09.26`, anonymised the same way.

**License:** MIT.

**What it exercises:** the durable projection: `<timestamp>`/`<user_query>` wrapped user text (minute-precision stamps, parsed into `ts`), `tool_use` blocks with no ids or results (Read, Write, GetDynamicTools, CallDynamicTool, CreatePlan), and the `turn_ended` control line.
