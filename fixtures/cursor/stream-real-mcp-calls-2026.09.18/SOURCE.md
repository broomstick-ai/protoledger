# stream-real-mcp-calls-2026.09.18

**Origin:** `src/daemon/agent/runners/fixtures/cursor/2026.09.18-mcp-calls.jsonl` in https://github.com/imshaikot/browsentic at commit `accf225a157f5e36bd56346febd29249af30c4af`, copied byte for byte.

Real capture from cursor-agent 2026.09.18-9a7762b (Auto model), recorded by browsentic's authors; their header comment lines (`# ...`) are part of the file and kept. They replaced the working directory with `/Users/you/...`; nothing else was edited.

**License:** Apache-2.0.

**What it exercises:** MCP calls (`mcpToolCall` with providerIdentifier/toolName), `getMcpTools`, an image MCP result (base64 PNG), an output spilled to a file, permission-denied shell calls, newline characters inside call ids, and non-JSON comment lines.
