"""Build the opencode fixtures whose container (or content) is assembled from
opencode's own schema, rather than copied from a store opencode wrote.

Usage: OPENCODE_REFS=<dir with a clone of kenn-io/agentsview> \
       python3 fixtures/opencode/_tools/mkfix.py <protoledger repo root> [maker ...]

Makers: agentsview, v1-storage, v2-transfer, v1-sqlite (needs v1-storage first).
Each SOURCE.md names the opencode source files a case was written from.
"""
import json, os, sqlite3, sys, shutil

ROOT = sys.argv[1]
FIX = os.path.join(ROOT, "fixtures/opencode")
SCHEMA_V2 = open(os.path.join(ROOT, "src/formats/opencode/schema_v2.sql")).read()
MIGRATIONS_V2 = [l.strip() for l in open(os.path.join(ROOT, "src/formats/opencode/migrations_v2.txt")) if l.strip()]
SCHEMA_V1 = open(os.path.join(ROOT, "src/formats/opencode/schema_v1.sql")).read()
REFS = os.environ.get("OPENCODE_REFS", ".")

# Field order of each SessionMessage struct (packages/schema/src/session-message.ts):
# where `id` and `type` sit among the encoded keys.
BASE = ["id", "metadata", "time"]
ORDER = {
    "agent-switched": BASE + ["type", "agent", "previous"],
    "model-switched": BASE + ["type", "model", "previous"],
    "location-switched": BASE + ["type", "location", "projectID", "subpath", "previous"],
    "user": BASE + ["text", "files", "agents", "skills", "type"],
    "synthetic": BASE + ["text", "description", "type"],
    "system": BASE + ["type", "text", "description"],
    "skill": BASE + ["type", "skill", "name", "text"],
    "shell": BASE + ["type", "shellID", "command", "status", "exit", "output"],
    "assistant": BASE + ["type", "agent", "model", "content", "snapshot", "finish", "rawFinish", "providerState", "cost", "tokens", "error", "retry"],
    "compaction": ["type"] + BASE + ["status", "reason", "model", "providerState", "summary", "recent", "providerContext", "cost", "tokens", "error"],
    "idle": BASE + ["type", "outcome", "error"],
}
INFO_ORDER = ["id", "parentID", "fork", "projectID", "agent", "model", "cost", "tokens", "outcome", "time", "title", "location", "subpath", "metadata", "permissions", "revert"]


def ordered(d, order):
    out = {k: d[k] for k in order if k in d}
    for k in d:
        if k not in out:
            out[k] = d[k]
    return out


def msg_from_row(r):
    data = r["data"] if isinstance(r["data"], dict) else json.loads(r["data"])
    m = dict(data)
    m["id"] = r["id"]
    m["type"] = r["type"]
    return ordered(m, ORDER.get(r["type"], BASE + ["type"]))


def write_json(path, v):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(json.dumps(v, indent=2, ensure_ascii=False) + "\n")


def write_json_noeol(path, v):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(json.dumps(v, indent=2, ensure_ascii=False))


def fresh(case):
    d = os.path.join(FIX, case)
    if os.path.exists(os.path.join(d, "native")):
        shutil.rmtree(os.path.join(d, "native"))
    os.makedirs(os.path.join(d, "native"))
    return d


def v2_db(path, project, sessions):
    """sessions: [(session_row_dict, [message_row_dict])]"""
    db = sqlite3.connect(path)
    db.executescript(SCHEMA_V2)
    db.execute("CREATE TABLE \"migration\" (id TEXT PRIMARY KEY, time_completed INTEGER NOT NULL)")
    t0 = min(s["time_created"] for s, _ in sessions)
    for m in MIGRATIONS_V2:
        db.execute("INSERT INTO migration VALUES (?, ?)", (m, t0))
    cols = ",".join(project)
    db.execute(f"INSERT INTO project ({cols}) VALUES ({','.join('?' * len(project))})", list(project.values()))
    for s, msgs in sessions:
        cols = ",".join(s)
        db.execute(f"INSERT INTO session_v2 ({cols}) VALUES ({','.join('?' * len(s))})", [json.dumps(v, separators=(',', ':')) if isinstance(v, (dict, list)) else v for v in s.values()])
        for r in msgs:
            data = r["data"] if isinstance(r["data"], str) else json.dumps(r["data"], separators=(',', ':'), ensure_ascii=False)
            db.execute("INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES (?,?,?,?,?,?,?)",
                       (r["id"], s["id"], r["type"], r["seq"], r["time_created"], r["time_updated"], data))
        if msgs:
            db.execute("INSERT INTO event_sequence (aggregate_id, seq, owner_id) VALUES (?,?,NULL)", (s["id"], max(r["seq"] for r in msgs)))
    db.commit()
    db.execute("VACUUM")
    db.close()


def v2_session_row(id, title, created, updated, version, slug, directory="/home/user/project-a", **kw):
    row = {
        "id": id, "project_id": "global", "workspace_id": None, "parent_id": None, "fork_session_id": None, "fork_boundary": None,
        "slug": slug, "directory": directory, "path": "", "title": title, "version": version, "share_url": None,
        "summary_additions": None, "summary_deletions": None, "summary_files": None, "summary_diffs": None, "metadata": None,
        "cost": 0.0, "tokens_input": 0, "tokens_output": 0, "tokens_reasoning": 0, "tokens_cache_read": 0, "tokens_cache_write": 0,
        "revert": None, "permission": None, "agent": "build", "model": {"id": "big-pickle", "providerID": "opencode"},
        "time_created": created, "time_updated": updated, "time_idle": updated, "time_viewed": None, "idle_outcome": "succeeded",
        "time_compacting": None, "time_archived": None, "time_suspended": None, "resume_attempts": 0,
    }
    row.update(kw)
    return row


def global_project(t):
    return {"id": "global", "worktree": "/", "vcs": None, "name": None, "icon_url": None, "icon_url_override": None, "icon_color": None,
            "time_created": t, "time_updated": t, "time_initialized": None, "time_active": t, "time_archived": None, "sandboxes": "[]", "commands": None}


def tokens_of(msgs):
    t = {"input": 0, "output": 0, "reasoning": 0, "read": 0, "write": 0}
    for r in msgs:
        d = r["data"] if isinstance(r["data"], dict) else json.loads(r["data"])
        tk = d.get("tokens")
        if r["type"] == "assistant" and tk:
            t["input"] += tk["input"]; t["output"] += tk["output"]; t["reasoning"] += tk["reasoning"]
            t["read"] += tk["cache"]["read"]; t["write"] += tk["cache"]["write"]
    return t


def agentsview_cases():
    base = os.path.join(REFS, "agentsview/internal/parser/testdata/opencode_v2")
    # 1. Rows captured from opencode 1.18.25's v2 session API, in a 2.x db.
    rows = json.load(open(os.path.join(base, "messages.json")))
    d = fresh("v2-sqlite-captured-1.18-read-tool")
    t = tokens_of(rows)
    created = rows[0]["time_created"] - 50
    updated = rows[-1]["time_updated"]
    sid = "ses_f794bb7a9ffeQ3m4KcJxW1yZtB"
    s = v2_session_row(sid, "Read input.txt", created, updated, "1.18.25", "brave-otter",
                       directory="/workspace/project-a", tokens_input=t["input"], tokens_output=t["output"], tokens_reasoning=t["reasoning"],
                       tokens_cache_read=t["read"], tokens_cache_write=t["write"])
    v2_db(os.path.join(d, "native/opencode.db"), global_project(created), [(s, rows)])
    # 2. Rows captured from the 2.0 beta (19381) prompt + compact APIs, as a session export.
    rows = json.load(open(os.path.join(base, "attachment_compaction.json")))
    d = fresh("v2-transfer-captured-beta-attachment-compaction")
    t = tokens_of(rows)
    created = rows[0]["time_created"] - 40
    sid = "ses_f789b931effeK8sPq2LmN4vRxW"
    info = {
        "id": sid, "projectID": "46959d5a6b1f1f358e3bb9ee11816810d84ed66e", "agent": "build",
        "model": {"id": "big-pickle", "providerID": "opencode", "variant": "default"}, "cost": 0,
        "tokens": {"input": t["input"], "output": t["output"], "reasoning": t["reasoning"], "cache": {"read": t["read"], "write": t["write"]}},
        "outcome": "succeeded", "time": {"created": created, "updated": rows[-1]["time_updated"], "idle": rows[-1]["time_updated"]},
        "title": "Attached file", "location": {"directory": "/workspace/project-a"},
    }
    write_json(os.path.join(d, "native", f"{sid}.json"), {"info": ordered(info, INFO_ORDER), "messages": [msg_from_row(r) for r in rows]})




# ---------------------------------------------------------------- synthetic
import hashlib

CH = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
_counter = {}


def oc_id(prefix, ms, seed, descending=False):
    """packages/schema/src/identifier.ts, with the random tail derived from `seed`."""
    c = _counter.get(ms, 0) + 1
    _counter[ms] = c
    v = ms * 0x1000 + c
    if descending:
        v = ~v
    v &= (1 << 48) - 1
    tail = "".join(CH[b % 62] for b in hashlib.sha256(seed.encode()).digest()[:14])
    return f"{prefix}_{v:012x}{tail}"


T0 = 1777716000000  # 2026-05-02T10:00:00Z
DIR = "/home/user/project-a"
B64_PLAN = "IyBQbGFuCgotIFJlYWQgdGhlIGlucHV0LgotIENvdW50IHRoZSBsaW5lcy4K"  # "# Plan\n\n- Read the input.\n- Count the lines.\n"


def v1_storage_synthetic():
    d = fresh("v1-storage-json-synthetic-subagent-compaction")
    st = os.path.join(d, "native/storage")
    pid = "4b0ea68d7af9a6031a7ffda7ad66e0cb83315750"
    ver = "1.1.8"
    sid = oc_id("ses", T0, "parent", True)
    cid = oc_id("ses", T0 + 9000, "child", True)
    tok = lambda i, o, r=0, cr=0, cw=0: {"input": i, "output": o, "reasoning": r, "cache": {"read": cr, "write": cw}}
    files = {}

    def put(rel, v):
        files[rel] = v

    put("migration", 2)
    put(f"project/{pid}.json", {"id": pid, "worktree": DIR, "vcs": "git", "time": {"created": T0 - 60000, "updated": T0 - 60000, "initialized": T0 - 59000}})
    put(f"session/{pid}/{sid}.json", {"id": sid, "version": ver, "projectID": pid, "directory": DIR, "title": "Summarise the plan and fix a typo",
                                       "time": {"created": T0, "updated": T0 + 60000}, "summary": {"additions": 1, "deletions": 1}})
    put(f"session/{pid}/{cid}.json", {"id": cid, "version": ver, "projectID": pid, "directory": DIR, "parentID": sid,
                                       "title": "Summarise plan.md (@general subagent)", "time": {"created": T0 + 9000, "updated": T0 + 15000}})
    put(f"session_diff/{sid}.json", [{"file": "notes/plan.md", "before": "# Plan\n\n- Read teh input.\n", "after": "# Plan\n\n- Read the input.\n", "additions": 1, "deletions": 1}])

    def msg(session, m):
        put(f"message/{session}/{m['id']}.json", m)

    def part(p):
        put(f"part/{p['messageID']}/{p['id']}.json", p)

    def P(mid, session, ms, kind, **kw):
        return {"id": oc_id("prt", ms, f"{mid}/{kind}/{ms}"), "sessionID": session, "messageID": mid, "type": kind, **kw}

    def user(session, ms, model="claude-sonnet-4-5", agent="build", **kw):
        return {"id": oc_id("msg", ms, f"{session}/u/{ms}"), "sessionID": session, "role": "user", "time": {"created": ms},
                "agent": agent, "model": {"providerID": "anthropic", "modelID": model}, **kw}

    def asst(session, parent, ms, done, cost, tokens, finish=None, **kw):
        m = {"id": oc_id("msg", ms, f"{session}/a/{ms}"), "sessionID": session, "role": "assistant", "time": {"created": ms, "completed": done},
             "parentID": parent, "modelID": "claude-sonnet-4-5", "providerID": "anthropic", "mode": "build", "agent": "build",
             "path": {"cwd": DIR, "root": DIR}, "cost": cost, "tokens": tokens, **kw}
        if finish:
            m["finish"] = finish
        return m

    # Parent turn 1: a prompt with an attached file, answered through the task tool.
    u1 = user(sid, T0 + 1000, summary={"diffs": []})
    msg(sid, u1)
    part(P(u1["id"], sid, T0 + 1000, "text", text="Summarise notes/plan.md with a subagent, then fix the typo in it."))
    part(P(u1["id"], sid, T0 + 1001, "file", mime="text/plain", filename="plan.md", url="data:text/plain;base64," + B64_PLAN,
           source={"type": "file", "path": "notes/plan.md", "text": {"value": "@notes/plan.md", "start": 0, "end": 14}}))
    a1 = asst(sid, u1["id"], T0 + 2000, T0 + 16000, 0.0123, tok(1820, 210, 0, 0, 1200), "tool-calls")
    msg(sid, a1)
    part(P(a1["id"], sid, T0 + 2000, "step-start", snapshot="5c1f0a3e9d0b8f7a6e5d4c3b2a1908f7e6d5c4b3"))
    part(P(a1["id"], sid, T0 + 2100, "reasoning", text="I should delegate the summary to the general agent.", metadata={"anthropic": {"signature": "EqQBCkYIBRgCKkBsynthetic"}},
           time={"start": T0 + 2100, "end": T0 + 2600}))
    part(P(a1["id"], sid, T0 + 2700, "tool", callID="toolu_01SynthTask", tool="task",
           state={"status": "completed", "input": {"description": "Summarise plan.md", "prompt": "Read notes/plan.md and summarise it in one line.", "subagent_type": "general"},
                  "output": "The plan reads the input and counts its lines.\n\n<task_metadata>\nsession_id: " + cid + "\n</task_metadata>",
                  "title": "Summarise plan.md", "metadata": {"summary": [], "sessionId": cid}, "time": {"start": T0 + 2700, "end": T0 + 15500}}))
    part(P(a1["id"], sid, T0 + 15900, "step-finish", reason="tool-calls", snapshot="5c1f0a3e9d0b8f7a6e5d4c3b2a1908f7e6d5c4b3", cost=0.0123, tokens=tok(1820, 210, 0, 0, 1200)))
    # Retry, then an edit with its patch.
    a2 = asst(sid, u1["id"], T0 + 17000, T0 + 24000, 0.0071, tok(2210, 140, 0, 1200, 0), "stop")
    msg(sid, a2)
    part(P(a2["id"], sid, T0 + 17000, "step-start", snapshot="5c1f0a3e9d0b8f7a6e5d4c3b2a1908f7e6d5c4b3"))
    part(P(a2["id"], sid, T0 + 17500, "retry", attempt=1, error={"name": "APIError", "data": {"message": "Overloaded", "statusCode": 529, "isRetryable": True}}, time={"created": T0 + 17500}))
    part(P(a2["id"], sid, T0 + 20000, "tool", callID="toolu_01SynthEdit", tool="edit",
           state={"status": "completed", "input": {"filePath": DIR + "/notes/plan.md", "oldString": "teh", "newString": "the"}, "output": "",
                  "title": "notes/plan.md", "metadata": {"diagnostics": {}, "diff": "@@ -3 +3 @@\n-- Read teh input.\n+- Read the input.\n", "filediff": {"file": DIR + "/notes/plan.md", "additions": 1, "deletions": 1}},
                  "time": {"start": T0 + 20000, "end": T0 + 20300}}))
    part(P(a2["id"], sid, T0 + 22000, "text", text="The plan reads the input and counts its lines. I fixed \"teh\" to \"the\".", time={"start": T0 + 22000, "end": T0 + 23000}))
    part(P(a2["id"], sid, T0 + 23500, "step-finish", reason="stop", snapshot="9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a0b", cost=0.0071, tokens=tok(2210, 140, 0, 1200, 0)))
    part(P(a2["id"], sid, T0 + 23600, "patch", hash="5c1f0a3e9d0b8f7a6e5d4c3b2a1908f7e6d5c4b3", files=[DIR + "/notes/plan.md"]))
    # A manual compaction: the user message carries the part, a summary message answers it.
    u2 = user(sid, T0 + 30000)
    msg(sid, u2)
    part(P(u2["id"], sid, T0 + 30000, "compaction", auto=False))
    a3 = asst(sid, u2["id"], T0 + 30500, T0 + 34000, 0.0042, tok(2400, 160), "stop", summary=True)
    a3["mode"] = "compaction"; a3["agent"] = "compaction"
    msg(sid, a3)
    part(P(a3["id"], sid, T0 + 31000, "text", text="## Summary\n- Delegated a summary of notes/plan.md to a subagent.\n- Fixed a typo in notes/plan.md.", time={"start": T0 + 31000, "end": T0 + 33900}))
    # A follow-up that is aborted.
    u3 = user(sid, T0 + 40000, model="claude-haiku-4-5", agent="plan")
    msg(sid, u3)
    part(P(u3["id"], sid, T0 + 40000, "text", text="Now list every file in notes/."))
    part(P(u3["id"], sid, T0 + 40001, "text", text="The user changed the agent to plan.", synthetic=True))
    a4 = asst(sid, u3["id"], T0 + 41000, T0 + 42000, 0, tok(0, 0), None, error={"name": "MessageAbortedError", "data": {"message": "The operation was aborted."}})
    a4["modelID"] = "claude-haiku-4-5"; a4["mode"] = "plan"; a4["agent"] = "plan"
    msg(sid, a4)
    part(P(a4["id"], sid, T0 + 41000, "step-start"))
    part(P(a4["id"], sid, T0 + 41500, "tool", callID="toolu_01SynthGlob", tool="glob",
           state={"status": "error", "input": {"pattern": "notes/*"}, "error": "Tool execution aborted", "time": {"start": T0 + 41500, "end": T0 + 41900}}))
    # The child session the task tool ran.
    cu = user(cid, T0 + 9100, agent="general")
    msg(cid, cu)
    part(P(cu["id"], cid, T0 + 9100, "text", text="Read notes/plan.md and summarise it in one line."))
    ca = asst(cid, cu["id"], T0 + 9500, T0 + 15000, 0.003, tok(900, 40), "stop")
    ca["mode"] = "general"; ca["agent"] = "general"
    msg(cid, ca)
    part(P(ca["id"], cid, T0 + 9500, "step-start"))
    part(P(ca["id"], cid, T0 + 10000, "tool", callID="toolu_01SynthRead", tool="read",
           state={"status": "completed", "input": {"filePath": DIR + "/notes/plan.md"}, "output": "<file>\n00001| # Plan\n00002| \n00003| - Read teh input.\n00004| - Count the lines.\n</file>",
                  "title": "notes/plan.md", "metadata": {"preview": "# Plan"}, "time": {"start": T0 + 10000, "end": T0 + 10100}}))
    part(P(ca["id"], cid, T0 + 14000, "text", text="The plan reads the input and counts its lines.", time={"start": T0 + 14000, "end": T0 + 14900}))
    part(P(ca["id"], cid, T0 + 14950, "step-finish", reason="stop", cost=0.003, tokens=tok(900, 40)))
    for rel, v in files.items():
        p = os.path.join(st, rel)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "w") as f:
            f.write(json.dumps(v, indent=2, ensure_ascii=False))


def v2_transfer_synthetic():
    """Every SessionMessage type of opencode 2.0.31, a subagent child and a fork, as `opencode session export` files."""
    d = fresh("v2-transfer-synthetic-all-message-types")
    ver = "2.0.31"
    T = T0 + 86400000
    sid = oc_id("ses", T, "v2-parent", True)
    cid = oc_id("ses", T + 20000, "v2-child", True)
    fid = oc_id("ses", T + 90000, "v2-fork", True)
    tok = lambda i, o, r=0, cr=0, cw=0: {"input": i, "output": o, "reasoning": r, "cache": {"read": cr, "write": cw}}
    model = {"id": "gpt-5.5", "providerID": "openai", "variant": "high"}
    M = lambda ms, seed: oc_id("msg", ms, seed)
    msgs = []
    add = lambda m: msgs.append(ordered(m, ORDER[m["type"]]))
    add({"id": M(T + 100, "a"), "time": {"created": T + 100}, "type": "model-switched", "model": model})
    add({"id": M(T + 200, "b"), "time": {"created": T + 200}, "type": "agent-switched", "agent": "build"})
    add({"id": M(T + 300, "c"), "time": {"created": T + 300}, "type": "system", "text": "You are running in /home/user/project-a. Today is 2026-05-03.", "description": "Environment"})
    add({"id": M(T + 400, "d"), "time": {"created": T + 400}, "type": "skill", "skill": "skill_counting", "name": "counting", "text": "<skill name=\"counting\">Count lines with wc -l.</skill>"})
    u1 = M(T + 1000, "e")
    add({"id": u1, "time": {"created": T + 1000}, "type": "user", "text": "Count the lines of @notes/plan.md with a subagent, then run the tests.",
         "files": [{"data": B64_PLAN, "mime": "text/plain", "source": {"type": "uri", "uri": "file:///home/user/project-a/notes/plan.md"}, "name": "plan.md",
                    "mention": {"start": 20, "end": 34, "text": "@notes/plan.md"}}],
         "agents": [{"name": "general"}]})
    add({"id": M(T + 1100, "f"), "time": {"created": T + 1100}, "type": "synthetic", "text": "Called the read tool with {\"path\":\"notes/plan.md\"}", "description": "Attached file"})
    a1 = M(T + 2000, "g")
    add({"id": a1, "time": {"created": T + 2000, "streamed": T + 29000, "completed": T + 29500}, "type": "assistant", "agent": "build", "model": model,
         "content": [
             {"type": "reasoning", "text": "Delegate the count, then run tests.", "state": {"openai": {"itemId": "rs_synthetic", "reasoningEncryptedContent": "gAAAAsynthetic"}},
              "time": {"created": T + 2100, "completed": T + 2600}},
             {"type": "tool", "id": "call_synthSubagent", "name": "subagent", "executed": True,
              "state": {"status": "completed", "input": {"agent": "general", "description": "Count plan lines", "prompt": "Count the lines in notes/plan.md."},
                        "content": [{"type": "text", "text": f"<subagent sessionID=\"{cid}\" state=\"completed\">\n4 lines\n</subagent>"}],
                        "metadata": {"sessionID": cid, "status": "completed"}},
              "time": {"created": T + 3000, "ran": T + 3100, "completed": T + 28000}},
             {"type": "text", "text": "The subagent counted 4 lines."},
         ],
         "snapshot": {"start": "snap_start_1", "end": "snap_end_1", "files": ["notes/plan.md"]},
         "finish": "stop", "cost": 0.0215, "tokens": tok(3200, 260, 128, 1024, 0)})
    add({"id": M(T + 30000, "h"), "time": {"created": T + 30000, "completed": T + 31000}, "type": "shell", "shellID": "sh_synthetic01", "command": "npm test",
         "status": "exited", "exit": 7, "output": {"output": "1 failing\n", "cursor": 10, "size": 10, "truncated": False}})
    a2 = M(T + 32000, "i")
    add({"id": a2, "time": {"created": T + 32000, "completed": T + 36000}, "type": "assistant", "agent": "build", "model": model,
         "content": [
             {"type": "tool", "id": "call_synthPatch", "name": "patch",
              "state": {"status": "error", "input": {"patch": "*** Begin Patch\n*** End Patch"}, "error": {"type": "tool.execution", "message": "patch did not apply"},
                        "content": [{"type": "text", "text": "patch did not apply"}]},
              "time": {"created": T + 33000, "ran": T + 33010, "completed": T + 33050}},
             {"type": "tool", "id": "call_synthImage", "name": "read",
              "state": {"status": "completed", "input": {"path": "plot.png"},
                        "content": [{"type": "text", "text": "Image read successfully"}, {"type": "file", "uri": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=", "mime": "image/png", "name": "plot.png"}]},
              "time": {"created": T + 34000, "completed": T + 34100}},
         ],
         "finish": "error", "cost": 0.004, "tokens": tok(3600, 80), "error": {"type": "provider.error", "message": "Upstream timeout", "status": 504}})
    add({"id": M(T + 37000, "j"), "time": {"created": T + 37000}, "type": "idle", "outcome": "failed", "error": {"type": "provider.error", "message": "Upstream timeout", "status": 504}})
    add({"id": M(T + 40000, "k"), "time": {"created": T + 40000}, "type": "location-switched", "location": {"directory": "/home/user/project-a/notes"},
         "subpath": "notes", "previous": {"location": {"directory": "/home/user/project-a"}}})
    add({"id": M(T + 41000, "l"), "time": {"created": T + 41000}, "type": "compaction", "status": "completed", "reason": "auto",
         "model": model, "summary": "## Goal\nCount lines in notes/plan.md and run tests.\n## Progress\n- 4 lines; tests fail (exit 7).", "recent": "[User]: Count the lines\n\n[Assistant]: The subagent counted 4 lines.",
         "cost": 0.002, "tokens": tok(5000, 300)})
    add({"id": M(T + 50000, "m"), "time": {"created": T + 50000}, "type": "compaction", "status": "failed", "reason": "manual", "error": {"type": "provider.invalid-request", "message": "Context too long"}})
    add({"id": M(T + 60000, "n"), "time": {"created": T + 60000}, "type": "idle", "outcome": "interrupted"})
    last = msgs[-1]["time"]["created"]
    info = {"id": sid, "projectID": "4b0ea68d7af9a6031a7ffda7ad66e0cb83315750", "agent": "build", "model": model, "cost": 0.0295,
            "tokens": tok(11800, 640, 128, 1024, 0), "outcome": "interrupted", "time": {"created": T, "updated": last, "idle": last},
            "title": "Count plan lines and run tests", "location": {"directory": DIR}, "metadata": {"ci": {"job": 42}}}
    write_json(os.path.join(d, "native", f"{sid}.json"), {"info": ordered(info, INFO_ORDER), "messages": msgs})
    # The subagent's own session.
    cm = []
    cu = M(T + 20100, "cu")
    cm.append(ordered({"id": cu, "time": {"created": T + 20100}, "type": "user", "text": "You are a subagent spawned by another session.\nCount the lines in notes/plan.md."}, ORDER["user"]))
    cm.append(ordered({"id": M(T + 21000, "ca"), "time": {"created": T + 21000, "completed": T + 27000}, "type": "assistant", "agent": "general", "model": model,
                       "content": [{"type": "tool", "id": "call_synthShell", "name": "shell", "state": {"status": "completed", "input": {"command": "wc -l notes/plan.md"},
                                    "content": [{"type": "text", "text": "4 notes/plan.md"}], "metadata": {"exit": 0}}, "time": {"created": T + 22000, "ran": T + 22010, "completed": T + 22500}},
                                   {"type": "text", "text": "4 lines"}], "finish": "stop", "cost": 0.001, "tokens": tok(800, 20)}, ORDER["assistant"]))
    cm.append(ordered({"id": M(T + 27100, "ci"), "time": {"created": T + 27100}, "type": "idle", "outcome": "succeeded"}, ORDER["idle"]))
    cinfo = {"id": cid, "parentID": sid, "projectID": "4b0ea68d7af9a6031a7ffda7ad66e0cb83315750", "agent": "general", "model": model, "cost": 0.001,
             "tokens": tok(800, 20), "outcome": "succeeded", "time": {"created": T + 20000, "updated": T + 27100, "idle": T + 27100},
             "title": "Count plan lines", "location": {"directory": DIR}, "metadata": {"ci": {"job": 42}}}
    write_json(os.path.join(d, "native", f"{cid}.json"), {"info": ordered(cinfo, INFO_ORDER), "messages": cm})
    # A fork through the first assistant step: the fork copies settled history, ids
    # `msg_<fork event id>_<seq>` (packages/core/src/session/projector.ts).
    evt = oc_id("msg", T + 90000, "fork-event")[4:]
    copied = []
    for seq, m in enumerate(msgs[:7], start=1):
        c = dict(m)
        c["id"] = f"msg_{evt}_{seq}"
        copied.append(ordered(c, ORDER[c["type"]]))
    copied.append(ordered({"id": M(T + 91000, "fu"), "time": {"created": T + 91000}, "type": "user", "text": "Instead, count the words."}, ORDER["user"]))
    finfo = {"id": fid, "fork": {"sessionID": sid, "boundary": {"type": "through", "messageID": a1}}, "projectID": "4b0ea68d7af9a6031a7ffda7ad66e0cb83315750",
             "agent": "build", "model": model, "cost": 0, "tokens": tok(0, 0), "time": {"created": T + 90000, "updated": T + 91000},
             "title": "Count plan lines and run tests (fork #1)", "location": {"directory": DIR}, "metadata": {"ci": {"job": 42}}}
    write_json(os.path.join(d, "native", f"{fid}.json"), {"info": ordered(finfo, INFO_ORDER), "messages": copied})


def v1_sqlite_synthetic():
    """The v1 storage fixture's sessions as opencode 1.2.0's JSON migration
    (packages/opencode/src/storage/json-migration.ts at v1.2.0) writes them into
    opencode.db, plus a todo list (session.sql.ts TodoTable)."""
    src = os.path.join(FIX, "v1-storage-json-synthetic-subagent-compaction/native/storage")
    d = fresh("v1-sqlite-synthetic-1.2-todo")
    db = sqlite3.connect(os.path.join(d, "native/opencode.db"))
    db.executescript(SCHEMA_V1)
    db.execute('CREATE TABLE IF NOT EXISTS "__drizzle_migrations" (\n\t\t\tid SERIAL PRIMARY KEY,\n\t\t\thash text NOT NULL,\n\t\t\tcreated_at numeric\n\t\t)')
    for i, (h, ts) in enumerate([("30e179e4c29b97cc6ee462ba7b7156113f2ee2d9b5ba302dac93c7d8baa60578", 1769552633000),
                                 ("d8dd043540753fd59122af8ee190f04677315c5d6d06356ae407472d5f9b6a12", 1770830228000),
                                 ("738a920a0fd2afd341bca9fe98cadb216084bd7c8523180e13286ef1e322eb50", 1770993676000)], start=1):
        db.execute("INSERT INTO __drizzle_migrations (id, hash, created_at) VALUES (?,?,?)", (i, h, ts))
    j = lambda v: None if v is None else json.dumps(v, separators=(",", ":"), ensure_ascii=False)
    for f in sorted(os.listdir(os.path.join(src, "project"))):
        p = json.load(open(os.path.join(src, "project", f)))
        db.execute("INSERT INTO project (id, worktree, vcs, name, icon_url, icon_color, time_created, time_updated, time_initialized, sandboxes, commands) VALUES (?,?,?,?,?,?,?,?,?,?,?)",
                   (p["id"], p.get("worktree", "/"), p.get("vcs"), p.get("name"), None, None, p["time"]["created"], p["time"]["updated"], p["time"].get("initialized"), j(p.get("sandboxes", [])), None))
    for pid in sorted(os.listdir(os.path.join(src, "session"))):
        for f in sorted(os.listdir(os.path.join(src, "session", pid))):
            x = json.load(open(os.path.join(src, "session", pid, f)))
            slug = "quiet-" + ("falcon" if x.get("parentID") else "harbor")
            db.execute("INSERT INTO session (id, project_id, parent_id, slug, directory, title, version, share_url, summary_additions, summary_deletions, summary_files, summary_diffs, revert, permission, time_created, time_updated, time_compacting, time_archived) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                       (x["id"], x["projectID"], x.get("parentID"), x.get("slug", slug), x.get("directory", ""), x.get("title", ""), "1.2.0",
                        None, (x.get("summary") or {}).get("additions"), (x.get("summary") or {}).get("deletions"), (x.get("summary") or {}).get("files"),
                        j((x.get("summary") or {}).get("diffs")), None, None, x["time"]["created"], x["time"]["updated"], None, None))
            for mf in sorted(os.listdir(os.path.join(src, "message", x["id"]))):
                m = json.load(open(os.path.join(src, "message", x["id"], mf)))
                data = {k: v for k, v in m.items() if k not in ("id", "sessionID")}
                db.execute("INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?,?,?,?,?)",
                           (m["id"], x["id"], m["time"]["created"], m["time"].get("completed", m["time"]["created"]), j(data)))
                pdir = os.path.join(src, "part", m["id"])
                for pf in sorted(os.listdir(pdir)) if os.path.isdir(pdir) else []:
                    p = json.load(open(os.path.join(pdir, pf)))
                    data = {k: v for k, v in p.items() if k not in ("id", "sessionID", "messageID")}
                    t = (p.get("time") or {}).get("start") or ((p.get("state") or {}).get("time") or {}).get("start") or m["time"]["created"]
                    db.execute("INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?,?,?,?,?,?)",
                               (p["id"], m["id"], x["id"], t, t, j(data)))
            if not x.get("parentID"):
                for pos, (content, status) in enumerate([("Summarise notes/plan.md", "completed"), ("Fix the typo", "completed"), ("List notes/", "pending")]):
                    db.execute("INSERT INTO todo (session_id, content, status, priority, position, time_created, time_updated) VALUES (?,?,?,?,?,?,?)",
                               (x["id"], content, status, "medium", pos, x["time"]["created"] + 1500, x["time"]["updated"]))
    db.commit()
    db.execute("VACUUM")
    db.close()


MAKERS = {"agentsview": lambda: agentsview_cases(), "v1-storage": lambda: v1_storage_synthetic(), "v2-transfer": lambda: v2_transfer_synthetic(), "v1-sqlite": lambda: v1_sqlite_synthetic()}

if __name__ == "__main__":
    for w in sys.argv[2:] or list(MAKERS):
        MAKERS[w]()
