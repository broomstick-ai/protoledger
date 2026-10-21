"""Write a synthetic cursor-agent chat store (store.db + meta.json).

usage: python3 make-store.py <out dir>

Synthetic, written from the documented layout (no Cursor code or data):
blob ids are sha256(data); message blobs are compact AI SDK messages, as
marcus/sidecar's docs/guides/deprecated/cursor-db-format-guide.md (MIT,
@2a2978a8) describes them (user text wrapped in <user_info>/<user_query>,
assistant id "1" with reasoning + signature + providerOptions, tool-call with
providerOptions.cursor.rawToolCallArgs, a role "tool" message per result);
the root (field 1 = message refs, field 2 = title/empty/preview subtree) and
meta row follow specstoryai/getspecstory's reconstruct.go (Apache-2.0,
@f5e1c728), which its authors validated with `cursor-agent --resume`.
"""
import hashlib, json, os, sqlite3, sys

out = sys.argv[1]
os.makedirs(out, exist_ok=True)
blobs = {}

def put(b):
    i = hashlib.sha256(b).hexdigest()
    blobs[i] = b
    return i

def pb(field, payload):
    tag, n, out = (field << 3) | 2, len(payload), bytearray()
    for v in (tag, n):
        while True:
            byte = v & 0x7F
            v >>= 7
            if v:
                out.append(byte | 0x80)
            else:
                out.append(byte)
                break
    return bytes(out) + payload

def js(o):
    return json.dumps(o, separators=(",", ":"), ensure_ascii=False).encode()

msgs = [
    {"role": "user", "content": "<user_info>\nOS Version: linux\nWorkspace Path: /home/user/project\n</user_info>\n\n<user_query>\nWhat does main.rs print?\n</user_query>"},
    {"id": "1", "role": "assistant", "content": [
        {"type": "reasoning", "text": "I should read src/main.rs.", "providerOptions": {"cursor": {"modelName": "claude-4.5-sonnet-thinking"}}, "signature": "c2lnbmF0dXJlLTE="},
        {"type": "text", "text": "Let me read it."},
        {"type": "tool-call", "toolCallId": "toolu_01", "toolName": "Read", "args": {"path": "/home/user/project/src/main.rs"}, "providerOptions": {"cursor": {"rawToolCallArgs": "{\"path\": \"/home/user/project/src/main.rs\"}"}}},
    ]},
    {"role": "tool", "id": "toolu_01", "content": [{"type": "tool-result", "toolName": "Read", "toolCallId": "toolu_01", "result": "fn main() {\n    println!(\"hello\");\n}\n", "isError": False}]},
    {"id": "1", "role": "assistant", "content": [
        {"type": "tool-call", "toolCallId": "toolu_02", "toolName": "Shell", "args": {"command": "cargo run"}, "providerOptions": {"cursor": {"rawToolCallArgs": "{\"command\": \"cargo run\"}"}}},
    ]},
    {"role": "tool", "id": "toolu_02", "content": [{"type": "tool-result", "toolName": "Shell", "toolCallId": "toolu_02", "result": [{"type": "text", "text": "error: could not find `Cargo.toml`"}], "isError": True}]},
    {"id": "1", "role": "assistant", "content": [{"type": "text", "text": "It prints `hello`."}]},
]
ids = [put(js(m)) for m in msgs]
title = put(pb(1, b"main.rs output") + pb(2, b"7d2c9a4e-0000-4000-8000-0000000000a1"))
empty = put(pb(3, b""))
preview = put(pb(1, pb(1, "It prints `hello`.".encode())))
node = put(pb(1, bytes.fromhex(title)) + pb(2, bytes.fromhex(empty)) + pb(2, bytes.fromhex(preview)))
root = put(b"".join(pb(1, bytes.fromhex(i)) for i in ids) + pb(2, bytes.fromhex(node)))
agent = "7d2c9a4e-1b3f-4c5d-8e6f-0a1b2c3d4e5f"
meta = js({"agentId": agent, "latestRootBlobId": root, "name": "main.rs output", "mode": "default", "createdAt": 1790000000000, "lastUsedModel": "claude-4.5-sonnet-thinking"})
db = os.path.join(out, "store.db")
if os.path.exists(db):
    os.remove(db)
c = sqlite3.connect(db)
c.execute("CREATE TABLE blobs (id TEXT PRIMARY KEY, data BLOB)")
c.execute("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT)")
for i in sorted(blobs):
    c.execute("INSERT INTO blobs VALUES (?, ?)", (i, blobs[i]))
c.execute("INSERT INTO meta VALUES ('0', ?)", (meta.hex(),))
c.commit()
c.close()
with open(os.path.join(out, "meta.json"), "w") as f:
    f.write(json.dumps({"schemaVersion": 1, "createdAtMs": 1790000000000, "hasConversation": True, "updatedAtMs": 1790000060000, "cwd": "/home/user/project"}, separators=(",", ":")))
