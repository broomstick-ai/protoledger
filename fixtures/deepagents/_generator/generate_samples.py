"""Generate real Deep Agents checkpoint DBs hermetically (fake chat model, no network LLM).

Runs the real `deepagents.create_deep_agent` graph (StateBackend, so files live in
the `files` channel) against a real `langgraph.checkpoint.sqlite.SqliteSaver`.
Config metadata mirrors what deepagents-code (dcode) stamps on each run
(agent_name / updated_at / cwd), see libs/code/deepagents_code/config.py.

Usage: venv/bin/python generate_samples.py OUTDIR
"""
from __future__ import annotations

import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

from deepagents import create_deep_agent  # noqa: E402
from deepagents.backends import StateBackend  # noqa: E402
from langchain_core.messages import AIMessage, HumanMessage  # noqa: E402
from langgraph.checkpoint.sqlite import SqliteSaver  # noqa: E402

from fake_chat_model import GenericFakeChatModel  # noqa: E402  (copied from deepagents tests)

USAGE = {"input_tokens": 120, "output_tokens": 30, "total_tokens": 150,
         "input_token_details": {"cache_read": 100}, "output_token_details": {"reasoning": 12}}
RESP_META = {"model_name": "claude-sonnet-4-6", "model_provider": "anthropic",
             "stop_reason": "end_turn", "id": "msg_fake_0001"}


def tc(name, args, cid):
    return {"name": name, "args": args, "id": cid, "type": "tool_call"}


def cfg(thread_id):
    return {
        "configurable": {"thread_id": thread_id},
        "metadata": {"agent_name": "agent", "dcode_agent_name": "agent",
                     "updated_at": "2026-10-16T12:00:00+00:00", "cwd": "/work/demo"},
    }


LEGACY = os.environ.get("LEGACY") == "1"  # deepagents<0.6: todos built in, StateBackend factory


def make_agent(model, saver, todos: bool):
    kw = {}
    if todos and not LEGACY:
        from langchain.agents.middleware import TodoListMiddleware  # opt-in since deepagents 0.7.0
        kw["middleware"] = [TodoListMiddleware()]
    backend = (lambda rt: StateBackend(rt)) if LEGACY else StateBackend()
    return create_deep_agent(model=model, checkpointer=saver, backend=backend, **kw)


def run(db: Path, thread_id: str, turns: list[tuple[str, list[AIMessage]]], todos: bool = False):
    for p in (db, db.with_name(db.name + "-wal"), db.with_name(db.name + "-shm")):
        p.unlink(missing_ok=True)
    scripted = [m for _, ms in turns for m in ms]
    model = GenericFakeChatModel(messages=iter(scripted))
    with SqliteSaver.from_conn_string(str(db)) as saver:
        agent = make_agent(model, saver, todos)
        for user, _ in turns:
            agent.invoke({"messages": [HumanMessage(content=user)]}, config=cfg(thread_id))
        # flush WAL into the main file so the .db is self-contained
        saver.conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")


def main(out: Path):
    out.mkdir(parents=True, exist_ok=True)

    # 1. simple chat
    run(out / "01_simple_chat.db", "019a0000-0000-7000-8000-000000000001", [
        ("Say hello.", [AIMessage(content="Hello! How can I help?", id="lc_run-ai-1",
                                  usage_metadata=USAGE, response_metadata=RESP_META)]),
    ])

    # 2. tool calls: write_file then read_file (files channel), parallel calls in one turn
    run(out / "02_tool_calls.db", "019a0000-0000-7000-8000-000000000002", [
        ("Create notes.txt and read it back.", [
            AIMessage(content="I'll write the file.", tool_calls=[
                tc("write_file", {"file_path": "/notes.txt", "content": "alpha\nbeta\n"}, "toolu_01"),
            ], usage_metadata=USAGE, response_metadata=RESP_META),
            AIMessage(content="", tool_calls=[
                tc("read_file", {"file_path": "/notes.txt"}, "toolu_02"),
                tc("ls", {"path": "/"}, "toolu_03"),
            ]),
            AIMessage(content="notes.txt contains alpha and beta."),
        ]),
    ])

    # 3. multi-turn with todos + files + edit
    run(out / "03_multiturn_todos_files.db", "019a0000-0000-7000-8000-000000000003", [
        ("Plan and write a haiku file.", [
            AIMessage(content="", tool_calls=[tc("write_todos", {"todos": [
                {"content": "Draft haiku", "status": "in_progress"},
                {"content": "Save haiku", "status": "pending"}]}, "toolu_10")]),
            AIMessage(content="", tool_calls=[tc("write_file", {
                "file_path": "/haiku.md", "content": "old pond\nfrog leaps in\nsplash\n"}, "toolu_11")]),
            AIMessage(content="", tool_calls=[tc("write_todos", {"todos": [
                {"content": "Draft haiku", "status": "completed"},
                {"content": "Save haiku", "status": "completed"}]}, "toolu_12")]),
            AIMessage(content="Saved /haiku.md."),
        ]),
        ("Change 'splash' to 'water sound'.", [
            AIMessage(content="", tool_calls=[tc("edit_file", {
                "file_path": "/haiku.md", "old_string": "splash", "new_string": "water sound"}, "toolu_13")]),
            AIMessage(content="Done."),
        ]),
    ], todos=True)

    # 4. reasoning content blocks (v1 standard + anthropic native thinking) + subagent task
    run(out / "04_reasoning_subagent.db", "019a0000-0000-7000-8000-000000000004", [
        ("Think, then delegate a lookup.", [
            AIMessage(content=[
                {"type": "thinking", "thinking": "User wants delegation.", "signature": "sig-abc"},
                {"type": "text", "text": "Delegating to a subagent."},
            ], tool_calls=[tc("task", {"description": "Find the answer to 2+2",
                                       "subagent_type": "general-purpose"}, "toolu_20")],
               usage_metadata=USAGE, response_metadata=RESP_META),
            # consumed by the general-purpose subagent
            AIMessage(content=[{"type": "reasoning", "reasoning": "Trivial arithmetic."},
                               {"type": "text", "text": "4"}]),
            AIMessage(content="The subagent says 4.",
                      additional_kwargs={"reasoning_content": "openai-compat style reasoning"}),
        ]),
    ])

    # 5. long thread: 26 turns = 52 updates to `messages` -> crosses snapshot_frequency=50
    run(out / "05_long_thread_snapshot.db", "019a0000-0000-7000-8000-000000000005", [
        (f"Turn {i}: echo {i}.", [AIMessage(content=f"Echo {i}.")]) for i in range(26)
    ])


if __name__ == "__main__":
    main(Path(sys.argv[1]))
