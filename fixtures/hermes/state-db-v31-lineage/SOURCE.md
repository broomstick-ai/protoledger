# state-db-v31-lineage

- **Origin:** generated on 2026-10-16 by running hermes-agent's own
  `hermes_state.SessionDB` (NousResearch/hermes-agent @
  774a49c5b4e306221a06a857431c6b1b5f77f0a1, v0.22.2, schema_version 31) in its
  own virtualenv with `HERMES_HOME` set to a throwaway directory. Script:
  `gen_head.py` below.
- **License:** the database layout is hermes-agent's (MIT, (c) 2025 Nous
  Research); the conversation content is synthetic, written for protoledger.
- **What it exercises:** a root session with a system prompt (deduplicated
  into `system_prompts`), tool calls with `reasoning_details` signatures,
  multimodal user content (`\x00json:` prefix, base64 image), a `delegate_task`
  subagent session (`source='subagent'`, `_delegate_from`), a compression
  continuation (`end_reason='compression'`, `_compressed_summary=1`), two
  rewound rows (`active=0`, the soft delete hermes' rewind performs, applied
  with SQL) and session token totals (also set with SQL).
- **Sanitisation:** none needed. The database was checkpointed and switched to
  `journal_mode=DELETE` so it is a single file.

```python
"""Generate a hermes state.db with hermes-agent's own SessionDB (run inside its venv)."""
import json, sys, sqlite3, os, shutil
from pathlib import Path
out = Path(sys.argv[1]); out.mkdir(parents=True, exist_ok=True)
home = out / 'home'; shutil.rmtree(home, ignore_errors=True); home.mkdir()
os.environ['HERMES_HOME'] = str(home)
import hermes_state
db = hermes_state.SessionDB(db_path=home / 'state.db')
P = '20261016_120000_a1b2c3'; C = '20261016_121500_d4e5f6'; K = '20261016_120500_0a0b0c'
t0 = 1792152000.0
db.create_session(P, 'cli', model='anthropic/claude-sonnet-4.5', model_config={'reasoning_effort': 'medium'},
                  system_prompt='You are Hermes, a helpful assistant.', cwd='/home/user/demo')
db.append_message(P, 'user', 'List the files here and show me the image.', timestamp=t0 + 1)
tc = [{'id': 'call_1', 'type': 'function', 'function': {'name': 'terminal', 'arguments': json.dumps({'command': 'ls'})}},
      {'id': 'call_2', 'type': 'function', 'function': {'name': 'read_file', 'arguments': json.dumps({'path': 'notes.md'})}}]
db.append_message(P, 'assistant', None, tool_calls=tc, finish_reason='tool_calls', reasoning='Run ls and read the notes.',
                  reasoning_content='Run ls and read the notes.',
                  reasoning_details=[{'type': 'reasoning.text', 'text': 'Run ls and read the notes.', 'signature': 'EqQBCkYIBxgCKkA=', 'format': 'anthropic-claude-v1', 'index': 0}],
                  timestamp=t0 + 2, token_count=42)
db.append_message(P, 'tool', '{"output": "a.txt\\nnotes.md\\npic.png", "exit_code": 0}', tool_call_id='call_1', tool_name='terminal', timestamp=t0 + 3)
db.append_message(P, 'tool', '# Notes\nBuy milk.', tool_call_id='call_2', tool_name='read_file', timestamp=t0 + 3.5)
db.append_message(P, 'assistant', 'There are three files: a.txt, notes.md and pic.png. The notes say: Buy milk.', finish_reason='stop', timestamp=t0 + 4, token_count=20)
mm = [{'type': 'text', 'text': 'What is in this picture?'},
      {'type': 'image_url', 'image_url': {'url': 'data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=='}}]
db.append_message(P, 'user', mm, timestamp=t0 + 5)
db.append_message(P, 'assistant', 'A single transparent pixel.', finish_reason='stop', timestamp=t0 + 6, token_count=7)
# delegate child (subagent)
db.create_session(K, 'subagent', model='anthropic/claude-haiku-4.5', model_config={'_delegate_from': P}, parent_session_id=P)
db.append_message(K, 'user', 'Summarize a.txt', timestamp=t0 + 7)
db.append_message(K, 'assistant', 'It is empty.', finish_reason='stop', timestamp=t0 + 8)
db.end_session(K, 'completed')
# compression continuation
db.end_session(P, 'compression')
db.create_session(C, 'cli', model='anthropic/claude-sonnet-4.5', model_config={'reasoning_effort': 'medium'},
                  system_prompt='You are Hermes, a helpful assistant.', parent_session_id=P, cwd='/home/user/demo')
db.append_message(C, 'user', '[CONTEXT COMPACTION] Earlier: listed three files and read notes.md (buy milk).', timestamp=t0 + 10, _compressed_summary=True)
db.append_message(C, 'user', 'Now delete b.txt', timestamp=t0 + 11)
db.append_message(C, 'assistant', 'There is no b.txt; nothing to delete.', finish_reason='stop', timestamp=t0 + 12)
db.append_message(C, 'user', 'Never mind, delete a.txt', timestamp=t0 + 13)
db.append_message(C, 'assistant', 'Deleting a.txt now.', finish_reason='stop', timestamp=t0 + 14)
for m in (db.set_session_title, ):
    m(P, 'File listing')
db.close()
# Rewind the last exchange of C the way hermes does (soft delete: active = 0).
con = sqlite3.connect(home / 'state.db')
con.execute("UPDATE messages SET active = 0 WHERE session_id = ? AND id >= (SELECT max(id) - 1 FROM messages WHERE session_id = ?)", (C, C))
con.execute("UPDATE sessions SET input_tokens = 1834, output_tokens = 69, cache_read_tokens = 1200, api_call_count = 3, estimated_cost_usd = 0.0071 WHERE id = ?", (P,))
con.commit()
con.execute('PRAGMA wal_checkpoint(TRUNCATE)')
con.execute('PRAGMA journal_mode=DELETE')
con.close()
shutil.copy(home / 'state.db', out / 'state.db')
print(open(home / 'state.db', 'rb').read(16))
```
