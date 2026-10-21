# legacy-session-json

- **Origin:** synthetic, written on 2026-10-16 with the removed writers' own
  serialisation calls (script below):
  - `session_<id>.json`: `run_agent.py` `_save_session_log` at
    NousResearch/hermes-agent c29b4f55d986bba18028da653ce0c2485383d348 (the last
    commit with it), which calls `atomic_json_write(path, entry, indent=2,
    default=str)` → `json.dump(..., indent=2, ensure_ascii=False)`, with the
    entry keys in the writer's order.
  - gateway `<id>.jsonl`: `gateway/session.py` `append_to_transcript` at tag
    v2026.4.16, `json.dumps(message, ensure_ascii=False) + "\n"`, first line the
    `session_meta` record from `gateway/run.py`.
- **License:** writer code MIT (c) 2025 Nous Research; content written for
  protoledger.
- **Unverified:** not captured from a running hermes; message dicts follow the
  OpenAI chat shape the agent kept in memory.
- **Sanitisation:** none needed.

```python
"""Legacy hermes session files, serialised exactly as the removed writers did:
run_agent.py _save_session_log (c29b4f55) -> atomic_json_write(indent=2, ensure_ascii=False, default=str)
gateway/session.py append_to_transcript (v2026.4.16) -> json.dumps(message, ensure_ascii=False) + "\n"."""
import json, sys
from pathlib import Path
out = Path(sys.argv[1]); (out / 'json').mkdir(parents=True, exist_ok=True); (out / 'jsonl').mkdir(parents=True, exist_ok=True)
sid = '20260410_101500_ab12cd'
tools = [{'type': 'function', 'function': {'name': 'web_search', 'description': 'Search the web.', 'parameters': {'type': 'object', 'properties': {'query': {'type': 'string'}}, 'required': ['query']}}}]
msgs = [
  {'role': 'user', 'content': 'Find the population of Reykjavík.'},
  {'role': 'assistant', 'content': '', 'reasoning': 'I should search for this.', 'finish_reason': 'tool_calls',
   'tool_calls': [{'id': 'call_ws1', 'type': 'function', 'function': {'name': 'web_search', 'arguments': '{"query": "Reykjavík population"}'}}]},
  {'role': 'tool', 'content': '{"results": [{"title": "Reykjavík", "snippet": "Population about 140,000."}]}', 'tool_call_id': 'call_ws1', 'tool_name': 'web_search'},
  {'role': 'assistant', 'content': '<think>\nThe snippet answers it.\n</think>\n\nReykjavík has about 140,000 inhabitants.', 'reasoning': 'The snippet answers it.', 'finish_reason': 'stop'},
]
entry = {'session_id': sid, 'model': 'anthropic/claude-sonnet-4', 'base_url': 'https://openrouter.ai/api/v1', 'platform': 'cli',
         'session_start': '2026-04-10T10:15:00.481203', 'last_updated': '2026-04-10T10:15:21.003310',
         'system_prompt': 'You are Hermes Agent, an intelligent AI assistant created by Nous Research.', 'tools': tools,
         'message_count': len(msgs), 'messages': msgs}
with open(out / 'json' / f'session_{sid}.json', 'w', encoding='utf-8') as f:
    json.dump(entry, f, indent=2, ensure_ascii=False, default=str)
gid = '20260412_080000_ef34ab'
lines = [
  {'role': 'session_meta', 'tools': tools, 'model': 'anthropic/claude-sonnet-4', 'platform': 'telegram', 'timestamp': '2026-04-12T08:00:00.120511'},
  {'role': 'user', 'content': 'Remind me what the weather is like in Oslo.', 'timestamp': '2026-04-12T08:00:00.120511'},
  {'role': 'assistant', 'content': None, 'tool_calls': [{'id': 'call_w1', 'type': 'function', 'function': {'name': 'web_search', 'arguments': '{"query": "Oslo weather today"}'}}], 'finish_reason': 'tool_calls', 'timestamp': '2026-04-12T08:00:04.930001'},
  {'role': 'tool', 'content': '{"results": [{"snippet": "Oslo: 7°C, light rain."}]}', 'tool_call_id': 'call_w1', 'tool_name': 'web_search', 'timestamp': '2026-04-12T08:00:06.410772'},
  {'role': 'assistant', 'content': 'Oslo is 7°C with light rain today. 🌧️', 'finish_reason': 'stop', 'timestamp': '2026-04-12T08:00:09.002145'},
]
with open(out / 'jsonl' / f'{gid}.jsonl', 'w', encoding='utf-8') as f:
    for m in lines:
        f.write(json.dumps(m, ensure_ascii=False) + '\n')
```
