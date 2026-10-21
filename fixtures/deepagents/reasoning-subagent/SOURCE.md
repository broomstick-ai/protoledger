# reasoning-subagent

- **Origin:** generated on 2026-10-16 by running the real
  `deepagents.create_deep_agent` graph (StateBackend) against a real
  `langgraph.checkpoint.sqlite.SqliteSaver`, with deepagents' own
  `GenericFakeChatModel` scripting the model replies (no network model).
  Environment: deepagents 0.7.28, langgraph 1.2.16, langgraph-checkpoint 4.3.0, langgraph-checkpoint-sqlite 3.1.2, langchain-core 1.6.12, ormsgpack 1.13.0 (Python 3.12, uv venv; full list in
  `../_generator/pip-freeze-current.txt`). Command: `venv/bin/python _generator/generate_samples.py OUT`; this is
  `04_reasoning_subagent.db`. Run metadata mirrors what deepagents-code stamps on each run
  (agent_name, dcode_agent_name, updated_at, cwd).
- **Sources read:** langchain-ai/deepagents @ e80fd67dfaf5ef19f11a64c5d039f18b8b0c7b27,
  langchain-ai/langgraph @ 41c9c22fdc83d68a2859d0c9a2b627c8615a792f (both MIT).
- **License:** the store layout is LangGraph's (MIT, (c) LangChain, Inc.); the
  conversation content is synthetic, written for protoledger.
- **What it exercises:** thinking/reasoning content blocks, `additional_kwargs.reasoning_content`, and a `task` subagent run under checkpoint namespace `tools:<task id>`.
- **Sanitisation:** none needed. The database was switched from WAL to
  `journal_mode=DELETE` (`PRAGMA journal_mode=delete`, which rewrites two
  header bytes) so it is a single self-contained file.
