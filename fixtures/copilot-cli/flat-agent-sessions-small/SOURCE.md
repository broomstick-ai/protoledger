# flat-agent-sessions-small

**Origin:** `Resources/Fixtures/stage0/agents/copilot/small.jsonl` in
https://github.com/jazzyalex/agent-sessions at commit
`474fcabf31d546434a77f15985c103cf03a347fc`, copied byte for byte and named like
a flat (0.0.342 - 0.0.377) session file, `session-state/<id>.jsonl`.

**License:** MIT.

**Sanitisation:** none; synthetic (ids like `e1`), written by agent-sessions'
authors for wide event-type coverage.

**What it exercises:** the flat variant; `session.resume`, `abort`, usage
records, `subagent.*`, unknown and drifted event types (kept as native).
