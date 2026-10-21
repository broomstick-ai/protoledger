# stream-real-composer2.5-2026-09

**Origin:** `results/cursor-composer-2.5/2026-09-09T23-56-58.157Z/agent-040-instant/run-1/transcript-raw.jsonl` in https://github.com/vercel/next-evals-oss at commit `8002355e2181cc34c07aec8a3d293939a637dc3e`, copied byte for byte.

Real capture on Composer 2.5, 2026-09-09, same harness and sandbox as above. Nothing removed.

**License:** MIT.

**What it exercises:** the 2026-09 `tool_call` shape (`hookAdditionalContexts`, `toolCallId`, string `startedAtMs`/`completedAtMs` siblings), assistant lines with both `timestamp_ms` and `model_call_id`, a `taskToolCall` subagent with `conversationSteps`, parallel reads.
