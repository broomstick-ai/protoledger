"""Write a synthetic Cursor IDE state.vscdb with one pre-_v-3 composer whose
bubbles are inline in composerData.conversation[], and one _v 3 composer with
bubbleId rows, tool calls (toolFormerData), thinking and tokenCount.

usage: python3 make-ide-inline.py <out file>

Synthetic, written from documented shapes (no Cursor code or data): the
composer/bubble fields as SimHacker/moollm's reverse-engineered
DATA-SCHEMAS.yml (MIT, @153f345a) and kenn-io/agentsview's cursor_ide.go
(MIT, @905a5fe2) read them. Values are compact JSON, as the IDE writes them.
"""
import json, os, sqlite3, sys

out = sys.argv[1]
if os.path.exists(out):
    os.remove(out)
js = lambda o: json.dumps(o, separators=(",", ":"), ensure_ascii=False)
c = sqlite3.connect(out)
c.execute("CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)")
c.execute("CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)")
old = "aaaaaaaa-0002-4000-8000-000000000002"
c.execute("INSERT INTO cursorDiskKV VALUES (?, ?)", (f"composerData:{old}", js({
    "composerId": old, "name": "Explain the build", "createdAt": 1735689600000, "lastUpdatedAt": 1735689660000,
    "conversation": [
        {"type": 1, "bubbleId": "b-user-1", "text": "How do I build this?", "richText": "", "relevantFiles": []},
        {"type": 2, "bubbleId": "b-asst-1", "text": "Run `cargo build`.", "codeBlocks": [], "modelInfo": {"modelName": "gpt-4o"}},
    ],
    "status": "completed",
})))
new = "bbbbbbbb-0003-4000-8000-000000000003"
bubbles = [
    {"_v": 3, "bubbleId": "u1", "type": 1, "text": "List the src directory.", "createdAt": "2026-03-01T10:00:00.000Z"},
    {"_v": 3, "bubbleId": "a1", "type": 2, "text": "", "createdAt": "2026-03-01T10:00:02.000Z", "thinking": {"text": "Use list_dir.", "signature": "c2ln"}, "modelInfo": {"modelName": "claude-4-sonnet"}},
    {"_v": 3, "bubbleId": "a2", "type": 2, "text": "", "createdAt": "2026-03-01T10:00:03.000Z", "capabilityType": 15,
     "toolFormerData": {"toolCallId": "toolu_x1", "tool": 5, "name": "list_dir", "rawArgs": "{\"relative_workspace_path\":\"src\"}", "params": "{\"directoryPath\":\"src\"}", "result": "{\"files\":[{\"name\":\"main.rs\"}]}", "status": "completed"}},
    {"_v": 3, "bubbleId": "a3", "type": 2, "text": "", "createdAt": "2026-03-01T10:00:04.000Z", "capabilityType": 15,
     "toolFormerData": {"toolCallId": "toolu_x2", "tool": 7, "name": "read_file", "rawArgs": "{\"target_file\":\"src/lib.rs\"}", "status": "error", "error": "File not found"}},
    {"_v": 3, "bubbleId": "a4", "type": 2, "text": "There is one file, `main.rs`.", "createdAt": "2026-03-01T10:00:05.000Z", "tokenCount": {"inputTokens": 1520, "outputTokens": 48}},
]
c.execute("INSERT INTO cursorDiskKV VALUES (?, ?)", (f"composerData:{new}", js({
    "_v": 3, "composerId": new, "name": "List src", "createdAt": 1772359200000, "lastUpdatedAt": 1772359205000,
    "fullConversationHeadersOnly": [{"bubbleId": b["bubbleId"], "type": b["type"]} for b in bubbles],
    "conversationMap": {}, "modelConfig": {"modelName": "claude-4-sonnet", "maxMode": False},
    "workspaceIdentifier": {"id": "ws1", "uri": {"fsPath": "/home/user/project"}}, "unifiedMode": "agent", "isAgentic": True,
})))
for b in bubbles:
    c.execute("INSERT INTO cursorDiskKV VALUES (?, ?)", (f"bubbleId:{new}:{b['bubbleId']}", js(b)))
c.commit()
c.close()
