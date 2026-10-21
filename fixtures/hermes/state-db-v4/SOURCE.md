# state-db-v4

- **Origin:** generated on 2026-10-16 with `hermes_state.py` from
  hermes-agent tag v2026.3.12 (a370ab8391ca5f8de7ebbc449f05cb0df36ade7c,
  schema_version 4, the first released state.db), run as-is (stdlib only).
  Script below.
- **License:** layout MIT (c) 2025 Nous Research; content synthetic.
- **What it exercises:** the oldest schema: inline FTS5 over `messages`, no
  reasoning columns, no `system_prompts` table (prompt on the session row),
  wall-clock timestamps written by that version's `append_message`.
- **Sanitisation:** none needed.

```python
"""Generate a schema-v4 state.db with hermes-agent v2026.3.12's own SessionDB."""
import json, sys, os, shutil, sqlite3
from pathlib import Path
sys.path.insert(0, str(Path(__file__).parent / 'v4src'))
out = Path(sys.argv[1]); shutil.rmtree(out, ignore_errors=True); out.mkdir(parents=True)
import hermes_state
db = hermes_state.SessionDB(db_path=out / 'state.db')
S = '20260315_093000_c0ffee'
db.create_session(S, 'cli', model='openai/gpt-5.2', model_config={'max_iterations': 60}, system_prompt='You are Hermes.')
db.append_message(S, 'user', 'What is 2+2? Check with python.')
tc = [{'id': 'call_9f2', 'type': 'function', 'function': {'name': 'execute_code', 'arguments': json.dumps({'code': 'print(2+2)'})}}]
db.append_message(S, 'assistant', None, tool_calls=tc, finish_reason='tool_calls', token_count=31)
db.append_message(S, 'tool', '{"status": "success", "output": "4\\n"}', tool_name='execute_code', tool_call_id='call_9f2')
db.append_message(S, 'assistant', '2 + 2 = 4.', finish_reason='stop', token_count=8)
db.update_token_counts(S, input_tokens=512, output_tokens=39)
db.set_session_title(S, 'Arithmetic check')
db.end_session(S, 'user_exit')
db.close()
con = sqlite3.connect(out / 'state.db'); con.execute('PRAGMA wal_checkpoint(TRUNCATE)'); con.execute('PRAGMA journal_mode=DELETE'); con.close()
for f in out.iterdir():
    if f.name != 'state.db': f.unlink()
```
