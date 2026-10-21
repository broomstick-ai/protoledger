# paginated-subagents-0.149.1

A real Codex CLI 0.149.1 paginated session: a root thread and three `thread_spawn` subagents (each also `forked_from_id` the root and carrying a copy of its session_meta), `item_completed`, `agent_message` inter-agent items, `inter_agent_communication_metadata`, `world_state`.

- **Origin:** https://github.com/furkankly/zoetrope/tree/b1f31dd26bd4e9e513885e39edb78d0850a5d1fe/assets/codex/cli-0.149.1
- **License:** MIT
- **Sanitised:** `/Users/demo` rewritten to `/home/user`. The publisher had already nulled some payloads (`world_state.state`, `turn_context` permission fields); Codex's loader skips those lines, the importer keeps them.
