# v2.0.22-inline-sidechain

Claude Code 2.0.22: a real session with an inline sidechain (the startup
"Warmup" agent's two lines sit in the session file with `isSidechain: true`
and no `agentId`), real thinking signatures and file-history snapshots.

## Origin and license

https://github.com/unixzii/agent-explorer, `src/fixtures/claude-transcript.sample.jsonl` at commit
57ff627e4229f7fd5867dcc1c89e01c60e15e0bf. License: MIT.

## Sanitisation

None needed: the cwd is `/` and no user paths occur. Renamed to `<sessionId>.jsonl`.
