use super::*;
use crate::formats::{ExportOptions, Format, ImportOptions, Output};
use crate::ledger::Ledger;
use crate::loss::{CompareOptions, compare};
use crate::model::{Agent, Event, Kind, Native, Obj, Parent, Relation, Source};
use serde_json::json;
use std::collections::{HashMap, HashSet};

fn agent(id: &str, parent: Option<(&str, &str)>) -> Agent {
    Agent {
        id: id.into(),
        created_at: Some("2026-10-16T21:00:00.000Z".into()),
        source: Some(Source { format: "codex".into(), variant: None, version: None, native_id: Some(id.into()), extra: Obj::new() }),
        parent: parent.map(|(a, e)| Parent { agent: a.into(), event: Some(e.into()), relation: Relation::Subagent }),
        links: vec![],
        meta: Obj::new(),
    }
}

fn ev(l: &mut Ledger, kind: Kind, body: serde_json::Value, group: Option<&str>) -> String {
    let n = l.events.len() + 1;
    let id = format!("{}-e{n}", l.id());
    let mut e = Event::new(id.clone(), kind).ts(Some(format!("2026-10-16T21:00:{n:02}.000Z"))).group(group);
    e.body = body.as_object().unwrap().clone();
    e.native = Some(Native::new("codex", Some(format!("item-{n}"))).record(json!({"i": n})));
    l.append(e).unwrap();
    id
}

/// A foreign (Codex-shaped) thread with a subagent and a compaction.
fn foreign_set() -> LedgerSet {
    let mut root = Ledger::new(agent("codex-root", None));
    ev(&mut root, Kind::Settings, json!({"cwd": "/w", "model": "gpt-5", "sandbox": "workspace-write"}), None);
    ev(&mut root, Kind::Text, json!({"role": "developer", "text": "be careful"}), None);
    ev(&mut root, Kind::Text, json!({"role": "user", "text": "count the files"}), None);
    ev(&mut root, Kind::Reasoning, json!({"text": "hm", "sealed": {"vendor": "openai", "kind": "encrypted", "data": "x"}}), Some("r1"));
    let spawn = ev(&mut root, Kind::ToolCall, json!({"call_id": "call.1/spawn", "name": "spawn agent", "input": {"task": "count"}}), Some("r1"));
    ev(&mut root, Kind::ToolCall, json!({"call_id": "call_2", "name": "apply_patch", "input_text": "*** Begin Patch"}), Some("r1"));
    ev(
        &mut root,
        Kind::Subagent,
        json!({"phase": "start", "agent": "codex-child", "call_id": "call.1/spawn", "agent_type": "worker", "description": "count"}),
        None,
    );
    ev(&mut root, Kind::ToolResult, json!({"call_id": "call.1/spawn", "output": "3 files"}), None);
    ev(&mut root, Kind::ToolResult, json!({"call_id": "call_2", "output": [{"type": "text", "text": "ok"}], "is_error": true}), None);
    ev(&mut root, Kind::ToolResult, json!({"call_id": "call_missing", "output": "stray"}), None);
    ev(&mut root, Kind::Text, json!({"role": "assistant", "text": "three"}), Some("r2"));
    ev(&mut root, Kind::Usage, json!({"model": "gpt-5", "input": 10, "output": 2}), None);
    ev(&mut root, Kind::Compaction, json!({"phase": "boundary", "trigger": "auto", "tokens_before": 9000}), None);
    ev(&mut root, Kind::Compaction, json!({"phase": "summary", "summary": "we counted files"}), None);
    ev(&mut root, Kind::Turn, json!({"phase": "end", "outcome": "ok", "duration_ms": 1200}), None);
    ev(&mut root, Kind::Title, json!({"text": "Counting"}), None);
    ev(&mut root, Kind::Error, json!({"message": "rate limited"}), None);
    let mut child = Ledger::new(agent("codex-child", Some(("codex-root", &spawn))));
    ev(&mut child, Kind::Text, json!({"role": "user", "text": "count"}), None);
    ev(&mut child, Kind::Text, json!({"role": "assistant", "text": "3"}), None);
    let mut set = LedgerSet::new();
    set.insert(root);
    set.insert(child);
    set
}

fn lines(out: &Output, key: &str) -> Vec<serde_json::Value> {
    String::from_utf8(out.files[key].clone()).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

fn reimport(out: &Output, opts: &ImportOptions) -> crate::formats::Imported {
    let dir = tempfile::tempdir().unwrap();
    out.write_to(dir.path(), true).unwrap();
    let r = ClaudeCode.import(dir.path(), opts).unwrap();
    drop(dir);
    r
}

#[test]
fn foreign_export_is_a_resumable_transcript() {
    let set = foreign_set();
    let ex = ClaudeCode.export(&set, &ExportOptions::no_carry()).unwrap();
    let sid = util_sid("codex-root");
    let main = format!("{sid}.jsonl");
    assert!(ex.output.files.contains_key(&main), "{:?}", ex.output.files.keys());
    let ls = lines(&ex.output, &main);
    // The parentUuid chain: each conversation line points at the one before;
    // a compaction boundary restarts it and names the old leaf logically.
    let mut prev: Option<String> = None;
    for l in ls.iter().filter(|l| l.get("uuid").is_some()) {
        assert_eq!(l["sessionId"], json!(sid));
        assert_eq!(l["cwd"], json!("/w"));
        assert!(l["version"].is_string() && l["timestamp"].is_string());
        if l["subtype"] == json!("compact_boundary") {
            assert!(l["parentUuid"].is_null());
            assert_eq!(l["logicalParentUuid"], json!(prev));
        } else {
            assert_eq!(l["parentUuid"], prev.clone().map(serde_json::Value::from).unwrap_or_default());
        }
        prev = l["uuid"].as_str().map(str::to_string);
    }
    // Uuids are minted deterministically.
    let again = ClaudeCode.export(&set, &ExportOptions::no_carry()).unwrap();
    assert_eq!(ex.output, again.output);
    // Tool use / result pairing with API-safe ids; one message for the r1 group.
    let uses: Vec<&serde_json::Value> =
        ls.iter().filter(|l| l["type"] == json!("assistant")).flat_map(|l| l["message"]["content"].as_array().unwrap()).collect();
    let ids: HashSet<&str> = uses.iter().filter(|b| b["type"] == json!("tool_use")).map(|b| b["id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.iter().all(|i| i.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')));
    assert!(uses.iter().any(|b| b["name"] == json!("spawn_agent")), "tool names are sanitised");
    assert!(uses.iter().any(|b| b["input"] == json!({"input_text": "*** Begin Patch"})), "free-text input is wrapped");
    let msg_ids: HashSet<&str> = ls
        .iter()
        .filter(|l| l["type"] == json!("assistant") && l["message"]["content"][0]["type"] == json!("tool_use"))
        .map(|l| l["message"]["id"].as_str().unwrap())
        .collect();
    assert_eq!(msg_ids.len(), 1, "blocks of one group share message.id");
    for r in ls.iter().filter(|l| l["message"]["content"][0]["type"] == json!("tool_result")) {
        let id = r["message"]["content"][0]["tool_use_id"].as_str().unwrap();
        assert!(ids.contains(id), "every tool_result answers a tool_use");
        assert!(r["sourceToolAssistantUUID"].is_string());
    }
    assert!(ls.iter().any(|l| l["message"]["content"].as_str().is_some_and(|t| t.starts_with("[tool result call_missing]"))));
    // Compaction, turn, title, error lines.
    assert!(ls.iter().any(|l| l["isCompactSummary"] == json!(true) && l["message"]["content"] == json!("we counted files")));
    assert!(ls.iter().any(|l| l["subtype"] == json!("turn_duration") && l["durationMs"] == json!(1200)));
    assert!(ls.iter().any(|l| l["type"] == json!("ai-title") && l["aiTitle"] == json!("Counting")));
    assert!(ls.iter().any(|l| l["subtype"] == json!("informational") && l["level"] == json!("error")));
    assert!(ls.iter().any(|l| l["isMeta"] == json!(true) && l["message"]["content"] == json!("be careful")));
    // Usage lands on the assistant message before it.
    let three = ls.iter().find(|l| l["message"]["content"][0]["text"] == json!("three")).unwrap();
    assert_eq!(three["message"]["usage"]["input_tokens"], json!(10));
    // The subagent: a sidecar transcript and meta file.
    let side: Vec<&String> = ex.output.files.keys().filter(|k| k.contains("/subagents/")).collect();
    assert_eq!(side.len(), 2, "{side:?}");
    let meta_key = side.iter().find(|k| k.ends_with(".meta.json")).unwrap();
    let meta: serde_json::Value = serde_json::from_slice(&ex.output.files[*meta_key]).unwrap();
    assert_eq!(meta["agentType"], json!("worker"));
    assert!(ids.contains(meta["toolUseId"].as_str().unwrap()), "meta names the spawning tool_use");
    let child = lines(&ex.output, side.iter().find(|k| k.ends_with(".jsonl")).unwrap());
    assert!(child.iter().all(|l| l["isSidechain"] == json!(true) && l["agentId"].is_string() && l["sessionId"] == json!(sid)));
    // Named losses.
    let codes: HashSet<&str> = ex.losses.0.iter().map(|l| l.code.as_str()).collect();
    for c in [
        "reasoning-sealed-dropped",
        "developer-as-meta-user",
        "tool-name-sanitised",
        "tool-input-text-as-custom",
        "orphan-tool-result-as-text",
        "settings-not-rendered",
    ] {
        assert!(codes.contains(c), "missing loss {c}: {codes:?}");
    }
    // Re-import: the subagent is a child again, tied to the spawning call.
    let back = reimport(&ex.output, &ImportOptions { variant: None, restore_carried: false });
    let kid = back.set.iter().find(|l| l.agent().parent.is_some()).unwrap();
    let p = kid.agent().parent.as_ref().unwrap();
    let parent = back.set.get(&p.agent).unwrap();
    let spawn = parent.events.iter().find(|e| Some(&e.id) == p.event.as_ref()).unwrap();
    assert_eq!(spawn.kind, Kind::ToolCall);
}

fn util_sid(id: &str) -> String {
    crate::util::uuid_for(id)
}

#[test]
fn carry_brings_foreign_events_back_exactly() {
    let set = foreign_set();
    let ex = ClaudeCode.export(&set, &ExportOptions::standard()).unwrap();
    let back = reimport(&ex.output, &ImportOptions::standard());
    let d = compare(&set, &back.set, CompareOptions::default());
    // Agent ids become Claude session/agent ids, but every event and native comes back.
    assert_eq!(d.missing.len() + d.extra.len() + d.changed.len(), 0, "{}", d.summary());
    // And re-exporting the restored set writes the same bytes.
    let ex2 = ClaudeCode.export(&back.set, &ExportOptions::standard()).unwrap();
    assert_eq!(ex.output, ex2.output);
}

#[test]
fn foreign_lines_after_native_ones_continue_the_chain() {
    let text = concat!(
        r#"{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":"hi"},"uuid":"u1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/p","sessionId":"s1","version":"2.1.280","gitBranch":"main"}"#,
        "\n"
    );
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("s1.jsonl"), text).unwrap();
    let mut set = ClaudeCode.import(dir.path(), &ImportOptions::standard()).unwrap().set;
    let l = set.ledgers.get_mut("s1").unwrap();
    l.append(Event::new("x1", Kind::Text).with("role", "assistant").with("text", "hello from elsewhere")).unwrap();
    let ex = ClaudeCode.export(&set, &ExportOptions::standard()).unwrap();
    let ls = lines(&ex.output, "s1.jsonl");
    assert_eq!(ls.len(), 2);
    // Compared as bytes: re-serialising the parsed line would sort its keys
    // (serde_json without `preserve_order`) and test serde_json, not the exporter.
    let first = String::from_utf8(ex.output.files["s1.jsonl"].clone()).unwrap().split_inclusive('\n').next().unwrap().to_string();
    assert_eq!(first, text, "the native line is untouched");
    assert_eq!(ls[1]["parentUuid"], json!("u1"));
    assert_eq!(ls[1]["sessionId"], json!("s1"));
    assert_eq!(ls[1]["cwd"], json!("/p"));
    assert_eq!(ls[1]["gitBranch"], json!("main"));
    assert_eq!(ls[1]["version"], json!("2.1.280"));
}

#[test]
fn usage_is_counted_once_per_message_from_the_line_that_settles_it() {
    // Two block lines of one message: the first (reasoning alone, so not the
    // last) carries partial streaming usage; the text line settles it.
    let a = |uuid: &str, parent: &str, block: serde_json::Value, out: u64| {
        json!({"parentUuid": parent, "isSidechain": false, "type": "assistant", "uuid": uuid, "timestamp": "2026-01-01T00:00:01.000Z",
               "sessionId": "s", "message": {"id": "msg_1", "model": "claude-x", "role": "assistant", "content": [block],
               "usage": {"input_tokens": 5, "output_tokens": out}}})
        .to_string()
    };
    let text = [
        json!({"parentUuid": null, "type": "user", "uuid": "u0", "sessionId": "s", "message": {"role": "user", "content": "q"}}).to_string(),
        a("a1", "u0", json!({"type": "thinking", "thinking": "", "signature": "sig"}), 3),
        a("a2", "a1", json!({"type": "text", "text": "ans"}), 40),
    ]
    .join("\n")
        + "\n";
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("s.jsonl"), &text).unwrap();
    let set = ClaudeCode.import(&dir.path().join("s.jsonl"), &ImportOptions::standard()).unwrap().set;
    let l = set.get("s").unwrap();
    let usage: Vec<&Event> = l.events.iter().filter(|e| e.kind == Kind::Usage).collect();
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].body["output"], json!(40));
    assert_eq!(usage[0].id, "msg_1#usage", "the usage event is keyed by the message id");
    assert_eq!(usage[0].native.as_ref().unwrap().id.as_deref(), Some("a2"), "a part of the line that settled it");
    let r = l.events.iter().find(|e| e.kind == Kind::Reasoning).unwrap();
    assert_eq!(r.body["sealed"], json!({"vendor": "anthropic", "kind": "signature", "data": "sig"}));
    assert_eq!(r.group.as_deref(), Some("msg_1"));
    // Settings: the model appears once.
    assert_eq!(l.events.iter().filter(|e| e.kind == Kind::Settings && e.body.contains_key("model")).count(), 1);
}

#[test]
fn persisted_tool_output_is_pulled_in_and_written_back() {
    let full = "line\n".repeat(1000);
    let res = json!({"parentUuid": null, "type": "user", "uuid": "u1", "sessionId": "s",
        "message": {"role": "user", "content": [{"tool_use_id": "toolu_1", "type": "tool_result",
            "content": "<persisted-output>\nOutput too large. Full output saved to: /home/user/.claude/projects/-w/s/tool-results/abc.txt\n\nPreview:\nline\n</persisted-output>"}]},
        "toolUseResult": {"stdout": "line", "persistedOutputPath": "/home/user/.claude/projects/-w/s/tool-results/abc.txt"}});
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("s.jsonl"), res.to_string() + "\n").unwrap();
    std::fs::create_dir_all(dir.path().join("s/tool-results")).unwrap();
    std::fs::write(dir.path().join("s/tool-results/abc.txt"), &full).unwrap();
    std::fs::write(dir.path().join("s/tool-results/orphan.bin"), [0xff, 0x00, 0xfe]).unwrap();
    std::fs::write(dir.path().join("s/unknown.bin"), [0xff, 0x01, 0xfe]).unwrap();
    let imp = ClaudeCode.import(dir.path(), &ImportOptions::standard()).unwrap();
    let l = imp.set.get("s").unwrap();
    let fc = l.events.iter().find(|e| e.kind == Kind::FileChange).expect("the persisted file is an event");
    assert_eq!(fc.body["content"], json!(full));
    assert_eq!(fc.body["path"], json!("/home/user/.claude/projects/-w/s/tool-results/abc.txt"));
    assert_eq!(fc.native.as_ref().unwrap().file.as_deref(), Some("s/tool-results/abc.txt"));
    assert!(l.events.iter().position(|e| e.kind == Kind::ToolResult) < l.events.iter().position(|e| e.kind == Kind::FileChange));
    let ex = ClaudeCode.export(&imp.set, &ExportOptions::standard()).unwrap();
    assert_eq!(ex.output.files["s/tool-results/abc.txt"], full.as_bytes());
    // A persisted output no line refers to yet is not read until one does
    // (SPEC §9.3: the line that refers to it may not be written yet).
    assert!(!ex.output.files.contains_key("s/tool-results/orphan.bin"));
    assert!(imp.losses.0.iter().any(|l| l.code == "tool-result-file-unreferenced"));
    // A file no line can refer to is kept, in a ledger of its own.
    assert!(imp.set.get("claude-code-files").is_some_and(|l| l.events.len() == 1));
    assert_eq!(ex.output.files["s/unknown.bin"], vec![0xff, 0x01, 0xfe], "binary sidecar files survive");
}

#[test]
fn inline_sidechains_split_into_ledgers_and_reinterleave() {
    let l = |v: serde_json::Value| v.to_string();
    let text = [
        l(json!({"parentUuid": null, "isSidechain": false, "type": "user", "uuid": "m1", "sessionId": "s", "message": {"role": "user", "content": "main"}})),
        l(json!({"parentUuid": null, "isSidechain": true, "type": "user", "uuid": "c1", "sessionId": "s", "message": {"role": "user", "content": "Warmup"}})),
        l(json!({"parentUuid": "m1", "isSidechain": false, "type": "assistant", "uuid": "m2", "sessionId": "s", "message": {"id": "x", "role": "assistant", "content": [{"type": "text", "text": "ok"}]}})),
        l(json!({"parentUuid": "c1", "isSidechain": true, "type": "assistant", "uuid": "c2", "sessionId": "s", "message": {"id": "y", "role": "assistant", "content": [{"type": "text", "text": "warm"}]}})),
        "not json".to_string(),
    ]
    .join("\n"); // and no final newline
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("s.jsonl"), &text).unwrap();
    let imp = ClaudeCode.import(&dir.path().join("s.jsonl"), &ImportOptions::standard()).unwrap();
    assert_eq!(imp.variant, V_INLINE);
    assert_eq!(imp.set.len(), 2);
    let side = imp.set.get("sidechain-c1").unwrap();
    assert_eq!(side.agent().parent.as_ref().unwrap().agent, "s");
    assert_eq!(side.events.iter().filter(|e| e.kind == Kind::Text).count(), 2);
    let ex = ClaudeCode.export(&imp.set, &ExportOptions::standard()).unwrap();
    assert_eq!(String::from_utf8(ex.output.files["s.jsonl"].clone()).unwrap(), text);
}

#[test]
fn edited_lines_are_imported_fresh() {
    let set = foreign_set();
    let ex = ClaudeCode.export(&set, &ExportOptions::standard()).unwrap();
    let key = format!("{}.jsonl", util_sid("codex-root"));
    let text = String::from_utf8(ex.output.files[&key].clone()).unwrap();
    let edited = text.replacen("count the files", "count the folders", 1);
    let mut out = ex.output.clone();
    out.put(key, edited.into_bytes());
    let back = reimport(&out, &ImportOptions::standard());
    let root = back.set.iter().find(|l| l.agent().parent.is_none()).unwrap();
    let t = root.events.iter().find(|e| e.str("text") == Some("count the folders")).unwrap();
    assert_eq!(t.native.as_ref().unwrap().format, FORMAT, "the edited line is a fresh Claude Code event");
    assert!(root.events.iter().all(|e| e.str("text") != Some("count the files")));
}

#[test]
fn task_notifications_parse() {
    let n = import::parse_task_notification(
        "<task-notification>\n<task-id>a2d</task-id>\n<tool-use-id>toolu_1</tool-use-id>\n<status>completed</status>\n<result>4 lines</result>\n</task-notification>",
    )
    .unwrap();
    assert_eq!(
        (n.task_id.as_str(), n.tool_use_id.as_deref(), n.status.as_deref(), n.result.as_deref()),
        ("a2d", Some("toolu_1"), Some("completed"), Some("4 lines"))
    );
    assert!(import::parse_task_notification("hello").is_none());
}

#[test]
fn detection_is_by_content() {
    let dir = tempfile::tempdir().unwrap();
    let codex = dir.path().join("rollout.jsonl");
    std::fs::write(&codex, r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"x"}}"#.to_string() + "\n").unwrap();
    assert!(ClaudeCode.detect(&codex).is_none());
    let cc = dir.path().join("s.jsonl");
    std::fs::write(
        &cc,
        r#"{"type":"summary","summary":"t","leafUuid":"u"}"#.to_string()
            + "\n"
            + r#"{"parentUuid":null,"type":"user","sessionId":"s","version":"2.0.70","uuid":"u","message":{"role":"user","content":"hi"}}"#
            + "\n",
    )
    .unwrap();
    let d = ClaudeCode.detect(&cc).unwrap();
    assert_eq!(d.version.as_deref(), Some("2.0.70"));
    assert_eq!(d.variant, V_FLAT);
    assert_eq!(slug_for("/private/tmp/pl-cc-resume"), "-private-tmp-pl-cc-resume");
    assert!(version_lt("2.1.9", "2.1.10") && !version_lt("2.1.10", "2.1.9"));
}

#[test]
fn line_ids_are_stable_and_unique() {
    // A repeated uuid in one file (seen after some resumes) still imports.
    let line = json!({"parentUuid": null, "type": "user", "uuid": "u1", "sessionId": "s", "message": {"role": "user", "content": "a"}}).to_string();
    let text = format!("{line}\n{line}\n");
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("s.jsonl"), &text).unwrap();
    let a = ClaudeCode.import(dir.path(), &ImportOptions::standard()).unwrap().set;
    let b = ClaudeCode.import(dir.path(), &ImportOptions::standard()).unwrap().set;
    assert_eq!(a, b);
    let ids: Vec<&str> = a.get("s").unwrap().events.iter().map(|e| e.id.as_str()).collect();
    let uniq: HashMap<&str, ()> = ids.iter().map(|i| (*i, ())).collect();
    assert_eq!(uniq.len(), ids.len());
    let ex = ClaudeCode.export(&a, &ExportOptions::standard()).unwrap();
    assert_eq!(ex.output.files["s.jsonl"], text.as_bytes());
}

/// A one-ledger foreign set built from `(kind, body, group)` triples.
fn foreign(events: &[(Kind, serde_json::Value, Option<&str>)]) -> LedgerSet {
    let mut l = Ledger::new(agent("f-root", None));
    for (k, b, g) in events {
        ev(&mut l, *k, b.clone(), *g);
    }
    let mut set = LedgerSet::new();
    set.insert(l);
    set
}

/// Export without carry and import again: the trip the loss score measures.
fn plain_trip(set: &LedgerSet) -> (crate::formats::Exported, LedgerSet) {
    let ex = ClaudeCode.export(set, &ExportOptions::no_carry()).unwrap();
    let back = reimport(&ex.output, &ImportOptions { variant: None, restore_carried: false });
    (ex, back.set)
}

fn all_lines(out: &Output) -> Vec<serde_json::Value> {
    out.files.keys().filter(|k| k.ends_with(".jsonl")).flat_map(|k| lines(out, k)).collect()
}

fn events_of(set: &LedgerSet) -> Vec<Event> {
    set.iter().find(|l| l.agent().parent.is_none()).unwrap().events.clone()
}

#[test]
fn unsigned_reasoning_becomes_a_thinking_block_with_an_empty_signature() {
    let set = foreign(&[
        (Kind::Text, json!({"role": "user", "text": "q"}), None),
        (Kind::Reasoning, json!({"text": "let me think"}), Some("r1")),
        (Kind::Reasoning, json!({"text": "sealed elsewhere", "sealed": {"vendor": "openai", "kind": "encrypted", "data": "x"}}), Some("r1")),
        (Kind::Reasoning, json!({"summary": ["only a summary"]}), Some("r1")),
        (Kind::Text, json!({"role": "assistant", "text": "a"}), Some("r1")),
    ]);
    let (ex, back) = plain_trip(&set);
    let blocks: Vec<serde_json::Value> =
        all_lines(&ex.output).iter().filter(|l| l["type"] == json!("assistant")).map(|l| l["message"]["content"][0].clone()).collect();
    assert_eq!(blocks[0], json!({"type": "thinking", "thinking": "let me think", "signature": ""}));
    assert_eq!(blocks[1], json!({"type": "thinking", "thinking": "sealed elsewhere", "signature": ""}));
    assert_eq!(blocks.len(), 3, "a summary with no text has no block");
    let codes: HashSet<&str> = ex.losses.0.iter().map(|l| l.code.as_str()).collect();
    assert!(codes.contains("reasoning-sealed-dropped") && codes.contains("reasoning-summary-dropped"), "{codes:?}");
    let r: Vec<Event> = events_of(&back).into_iter().filter(|e| e.kind == Kind::Reasoning).collect();
    assert_eq!(r.len(), 2);
    assert_eq!(r[0].str("text"), Some("let me think"));
    assert!(r[0].body.get("sealed").is_none(), "an empty signature is no seal");
}

#[test]
fn incoming_peer_messages_use_the_spec_wrapper_both_ways() {
    let set = foreign(&[
        (Kind::PeerMessage, json!({"direction": "in", "peer": "/root/a", "network": "team", "text": "hello\nthere"}), None),
        (Kind::PeerMessage, json!({"direction": "in", "text": "no attributes"}), None),
    ]);
    let (ex, back) = plain_trip(&set);
    let texts: Vec<String> = all_lines(&ex.output).iter().filter_map(|l| l["message"]["content"].as_str().map(str::to_string)).collect();
    assert_eq!(texts[0], "<peer-message direction=\"in\" peer=\"/root/a\" network=\"team\">\nhello\nthere\n</peer-message>");
    assert_eq!(texts[1], "<peer-message direction=\"in\">\nno attributes\n</peer-message>");
    let p: Vec<Event> = events_of(&back).into_iter().filter(|e| e.kind == Kind::PeerMessage).collect();
    assert_eq!(p.len(), 2);
    assert_eq!((p[0].str("peer"), p[0].str("network"), p[0].str("text")), (Some("/root/a"), Some("team"), Some("hello\nthere")));
    assert_eq!((p[1].str("peer"), p[1].str("text")), (None, Some("no attributes")));
    // Text that only resembles the wrapper stays text.
    assert!(peer_unwrap("<peer-message direction=\"in\" bogus=\"x\">\nt\n</peer-message>").is_none());
    assert!(peer_unwrap("<peer-message direction=\"in\">\nt\n</peer-message>\n").is_none());
}

#[test]
fn unknown_environment_is_left_out_not_invented() {
    let set = foreign(&[
        (Kind::Text, json!({"role": "user", "text": "q"}), None),
        (Kind::Text, json!({"role": "assistant", "text": "a"}), Some("r1")),
        (Kind::Compaction, json!({"phase": "boundary", "trigger": "auto"}), None),
    ]);
    let (ex, back) = plain_trip(&set);
    for l in all_lines(&ex.output).iter().filter(|l| l.get("uuid").is_some()) {
        assert!(l.get("cwd").is_none(), "no cwd was known: {l}");
        assert!(l["message"].get("model").is_none(), "no model was known: {l}");
        assert_eq!(l["gitBranch"], json!(""));
        if l["subtype"] == json!("compact_boundary") {
            assert!(l["compactMetadata"].get("preTokens").is_none());
        }
    }
    assert!(events_of(&back).iter().all(|e| e.kind != Kind::Settings), "nothing invented comes back as settings");
}

#[test]
fn an_empty_git_branch_is_no_setting_until_it_replaces_one() {
    let line = |u: &str, b: &str| {
        json!({"parentUuid": null, "type": "user", "uuid": u, "sessionId": "s", "cwd": "/p", "gitBranch": b,
               "message": {"role": "user", "content": "q"}})
        .to_string()
    };
    let text = [line("u1", ""), line("u2", "main"), line("u3", "")].join("\n") + "\n";
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("s.jsonl"), text).unwrap();
    let set = ClaudeCode.import(dir.path(), &ImportOptions::standard()).unwrap().set;
    let s: Vec<Option<String>> =
        set.get("s").unwrap().events.iter().filter(|e| e.kind == Kind::Settings).map(|e| e.str("git_branch").map(str::to_string)).collect();
    assert_eq!(s, vec![None, Some("main".into()), Some("".into())]);
}

#[test]
fn free_text_tool_calls_come_back_as_custom_calls() {
    let set = foreign(&[
        (Kind::Text, json!({"role": "user", "text": "patch it"}), None),
        (Kind::ToolCall, json!({"call_id": "c1", "name": "apply_patch", "tool_type": "custom", "input_text": "*** Begin Patch"}), Some("r1")),
        (Kind::ToolCall, json!({"call_id": "c2", "name": "edit", "input": {"input": "a JSON field named input"}}), Some("r1")),
        (Kind::ToolCall, json!({"call_id": "c3", "name": "shell", "input": "{\"command\":\"ls\"}"}), Some("r1")),
    ]);
    let (ex, back) = plain_trip(&set);
    let inputs: Vec<serde_json::Value> =
        all_lines(&ex.output).iter().filter(|l| l["type"] == json!("assistant")).map(|l| l["message"]["content"][0]["input"].clone()).collect();
    assert_eq!(inputs, vec![json!({"input_text": "*** Begin Patch"}), json!({"input": "a JSON field named input"}), json!({"command": "ls"})]);
    let calls: Vec<Event> = events_of(&back).into_iter().filter(|e| e.kind == Kind::ToolCall).collect();
    assert_eq!((calls[0].str("tool_type"), calls[0].str("input_text")), (Some("custom"), Some("*** Begin Patch")));
    assert!(calls[0].body.get("input").is_none());
    assert_eq!(calls[1].body.get("input"), Some(&json!({"input": "a JSON field named input"})), "a JSON tool stays one");
    assert_eq!(calls[1].str("tool_type"), Some("function"));
    let codes: HashSet<&str> = ex.losses.0.iter().map(|l| l.code.as_str()).collect();
    assert!(codes.contains("tool-input-parsed") && !codes.contains("tool-input-text-as-custom"), "{codes:?}");
}

#[test]
fn cost_state_totals_name_only_the_counts_they_state() {
    let text = [
        json!({"type": "cost-state", "sessionId": "s", "totalCostUSD": 0.5,
               "modelUsage": {"claude-x": {"inputTokens": 10, "outputTokens": 4, "thinkingTokens": 3}}})
        .to_string(),
        json!({"type": "cost-state", "sessionId": "s", "totalCostUSD": 0, "modelUsage": {}}).to_string(),
    ]
    .join("\n")
        + "\n";
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("s.jsonl"), text).unwrap();
    let set = ClaudeCode.import(dir.path(), &ImportOptions::standard()).unwrap().set;
    let u: Vec<&Event> = set.get("s").unwrap().events.iter().filter(|e| e.kind == Kind::Usage).collect();
    assert_eq!(u[0].body.get("reasoning"), Some(&json!(3)), "thinkingTokens are the reasoning subset");
    assert!(u[0].body.get("cache_read").is_none() && u[0].body.get("cache_write").is_none(), "unstated counts are unknown");
    assert_eq!(u[0].str("model"), Some("claude-x"));
    assert_eq!(u[1].body.get("input"), Some(&json!(0)), "an empty modelUsage states zero");
    assert!(u[1].body.get("reasoning").is_none());
}

#[test]
fn orphan_tool_results_come_back_from_their_user_text() {
    let set = foreign(&[
        (Kind::ToolResult, json!({"call_id": "gone", "output": "late output"}), None),
        (Kind::ToolResult, json!({"output": "no call at all"}), None),
    ]);
    let (ex, back) = plain_trip(&set);
    let texts: Vec<String> = all_lines(&ex.output).iter().filter_map(|l| l["message"]["content"].as_str().map(str::to_string)).collect();
    assert_eq!(texts, vec!["[tool result gone]\nlate output".to_string(), "[tool result ]\nno call at all".to_string()]);
    let r: Vec<Event> = events_of(&back).into_iter().filter(|e| e.kind == Kind::ToolResult).collect();
    assert_eq!((r[0].str("call_id"), r[0].str("output")), (Some("gone"), Some("late output")));
    assert_eq!((r[1].str("call_id"), r[1].str("output")), (None, Some("no call at all")));
    assert!(orphan_result_unwrap("[tool result a\nb]\nx").is_none());
}

#[test]
fn an_anthropic_seal_over_readable_text_is_a_signature() {
    let set = foreign(&[
        (Kind::Text, json!({"role": "user", "text": "q"}), None),
        (Kind::Reasoning, json!({"text": "visible", "sealed": {"vendor": "anthropic", "kind": "encrypted", "data": "EtSIG"}}), Some("r")),
        (Kind::Reasoning, json!({"redacted": true, "sealed": {"vendor": "anthropic", "kind": "encrypted", "data": "ENC"}}), Some("r")),
    ]);
    let (ex, _) = plain_trip(&set);
    let blocks: Vec<serde_json::Value> =
        all_lines(&ex.output).iter().filter(|l| l["type"] == json!("assistant")).map(|l| l["message"]["content"][0].clone()).collect();
    assert_eq!(blocks[0], json!({"type": "thinking", "thinking": "visible", "signature": "EtSIG"}));
    assert_eq!(blocks[1], json!({"type": "redacted_thinking", "data": "ENC"}));
}

#[test]
fn mcp_tools_take_claude_codes_prefixed_name() {
    let set = foreign(&[
        (Kind::Text, json!({"role": "user", "text": "q"}), None),
        (Kind::ToolCall, json!({"call_id": "c1", "name": "agents_spawn", "tool_type": "mcp", "server": "threads", "input": {}}), Some("r")),
    ]);
    let (ex, back) = plain_trip(&set);
    assert!(all_lines(&ex.output).iter().any(|l| l["message"]["content"][0]["name"] == json!("mcp__threads__agents_spawn")));
    let c = events_of(&back).into_iter().find(|e| e.kind == Kind::ToolCall).unwrap();
    assert_eq!((c.str("tool_type"), c.str("server")), (Some("mcp"), Some("threads")));
}

#[test]
fn an_unknown_time_is_written_as_the_epoch_and_read_back_as_unknown() {
    let mut l = Ledger::new(Agent { created_at: None, ..agent("f-root", None) });
    l.append(Event::new("e1", Kind::Text).with("role", "user").with("text", "q")).unwrap();
    let mut set = LedgerSet::new();
    set.insert(l);
    let (ex, back) = plain_trip(&set);
    assert_eq!(all_lines(&ex.output)[0]["timestamp"], json!(UNKNOWN_TS));
    let root = back.iter().next().unwrap();
    assert!(root.agent().created_at.is_none() && root.events.iter().all(|e| e.ts.is_none()));
}

/// Lines spelled the way a parsed `Value` does not re-spell them: unsorted
/// keys, `1e3` and `1.50`, the escape `\u00e9`, whitespace between tokens.
const ODD_LINES: [&str; 2] = [
    r#"{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":"caf\u00e9 ok"},"uuid":"u1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/p","sessionId":"s1","version":"2.1.280","gitBranch":"main"}"#,
    r#"{"parentUuid":"u1","isSidechain":false,"type": "assistant","message":{"id":"msg_1","model":"claude-x","role":"assistant","content":[{"type":"text","text":"ol\u00e1"}],"usage":{"input_tokens":5,"output_tokens":7}},"uuid":"a1","timestamp":"2026-01-01T00:00:01.000Z","cwd":"/p","sessionId":"s1","version":"2.1.280","gitBranch":"main","costUSD":1e3,"ratio":1.50}"#,
];

/// Opposes the test below: a line whose record cannot be held as its text
/// (a JSON value with trailing whitespace is held trimmed) still keeps `raw`.
#[test]
fn a_line_its_record_cannot_hold_keeps_raw() {
    let line = format!("{} ", ODD_LINES[0]);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s1.jsonl");
    std::fs::write(&path, format!("{line}\n")).unwrap();
    let set = ClaudeCode.import(&path, &ImportOptions::standard()).unwrap().set;
    let n = set.get("s1").unwrap().events.iter().filter_map(|e| e.native.as_ref()).find(|n| n.part == 0).unwrap();
    assert_eq!((n.record_text(), n.raw.as_deref()), (Some(ODD_LINES[0]), Some(line.as_str())));
    let st = crate::roundtrip::self_trip(&ClaudeCode, &path).unwrap();
    assert!(st.files.is_exact(), "self trip not exact: differ {:?}", st.files.differ);
}

#[test]
fn self_trip_reemits_odd_spellings_exactly() {
    let text = ODD_LINES.join("\n") + "\n";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s1.jsonl");
    std::fs::write(&path, &text).unwrap();
    // Each line is held as its own text, and only as that: carriers keep a
    // record's exact text, so no second copy in `raw` is emitted.
    let set = ClaudeCode.import(&path, &ImportOptions::standard()).unwrap().set;
    let natives: Vec<&Native> = set.get("s1").unwrap().events.iter().filter_map(|e| e.native.as_ref()).filter(|n| n.part == 0).collect();
    assert_eq!(natives.iter().map(|n| n.record_text().unwrap()).collect::<Vec<_>>(), ODD_LINES.to_vec());
    assert_eq!(
        natives.iter().map(|n| n.raw.as_deref()).collect::<Vec<_>>(),
        vec![None; ODD_LINES.len()],
        "a record held as its exact text also kept a raw copy"
    );
    // X → P → X is byte-exact.
    let st = crate::roundtrip::self_trip(&ClaudeCode, &path).unwrap();
    assert!(st.files.is_exact(), "self trip not exact: differ {:?} missing {:?} extra {:?}", st.files.differ, st.files.missing, st.files.extra);
    // ... and so is X → P → Y → P → X with carry, through the other primaries.
    for via in ["codex", "opencode"] {
        let y = crate::formats::by_name(via).unwrap();
        let ct = crate::roundtrip::cross_trip(&ClaudeCode, &path, y, true).unwrap();
        assert!(ct.back_files.is_exact(), "carry trip via {via} not exact: differ {:?}", ct.back_files.differ);
    }
}

// ---- branches (SPEC §2.3) ----

fn fixture(rel: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(rel)
}

const FLAT_SESSION: &str = "claude-code/v2.1.1-flat-subagent/native/39bfb6c7-6a1d-4a87-aea7-23ed53c2dd1d.jsonl";
/// The `system` line that ends the second turn of that session; its third
/// prompt (`commit`) continues from it.
const TURN_END: &str = "c8851889";

/// The flat-subagent session (a real 2.1.1 transcript) after a `/rewind` to
/// before its `commit` prompt: Claude Code appends, so the new prompt is a
/// line whose `parentUuid` goes back to the line that prompt continued from,
/// and the rewound lines stay in the file. Returns (dir, file, new uuids).
fn rewound_session() -> (tempfile::TempDir, std::path::PathBuf, [String; 2]) {
    let text = std::fs::read_to_string(fixture(FLAT_SESSION)).unwrap();
    let lines: Vec<serde_json::Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let prompt = lines.iter().find(|v| v["message"]["content"] == json!("commit")).unwrap();
    assert!(prompt["parentUuid"].as_str().unwrap().starts_with(TURN_END));
    let answer = lines.iter().rev().find(|v| v["type"] == "assistant" && v["message"]["content"][0]["type"] == "text").unwrap();
    let (u, a) = ("5d8b0a7e-4c1e-4f0e-9a51-0e7c2f3a9b10".to_string(), "5d8b0a7e-4c1e-4f0e-9a51-0e7c2f3a9b11".to_string());
    let mut user = prompt.clone();
    user["uuid"] = json!(u);
    user["message"]["content"] = json!("Do not commit yet; run the tests first.");
    user["timestamp"] = json!("2026-01-02T00:00:00.000Z");
    let mut asst = answer.clone();
    asst["parentUuid"] = json!(u);
    asst["uuid"] = json!(a);
    asst["message"]["id"] = json!("msg_rewound");
    asst["message"]["content"] = json!([{"type": "text", "text": "Running the tests first."}]);
    asst["timestamp"] = json!("2026-01-02T00:00:01.000Z");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(fixture(FLAT_SESSION).file_name().unwrap());
    let mut out = text.clone();
    for v in [&user, &asst] {
        out.push_str(&serde_json::to_string(v).unwrap());
        out.push('\n');
    }
    std::fs::write(&path, out).unwrap();
    (dir, path, [u, a])
}

fn session_ledger(path: &std::path::Path) -> Ledger {
    let set = ClaudeCode.import(path, &ImportOptions::standard()).unwrap().set;
    set.iter().find(|l| !l.id().starts_with("agent-")).unwrap().clone()
}

/// (c) A rewind becomes an `after` branch: every event of the rewound lines
/// stays in the ledger, unchanged, and the active path skips them.
#[test]
fn a_rewind_is_an_after_branch_that_keeps_the_abandoned_lines() {
    let original = session_ledger(&fixture(FLAT_SESSION));
    let (_dir, path, [u, a]) = rewound_session();
    let l = session_ledger(&path);
    assert_eq!(l.events[..original.events.len()], original.events[..], "the rewound lines' events are all kept, unchanged");
    let resume = l.events.iter().find(|e| e.id == u).unwrap();
    let turn_end = original.events.iter().rposition(|e| e.id.starts_with(TURN_END)).unwrap();
    assert_eq!(resume.after.as_deref(), Some(original.events[turn_end].id.as_str()));
    assert!(l.is_branched());
    let active: Vec<&str> = l.active_path().iter().map(|e| e.id.as_str()).collect();
    let mut want: Vec<&str> = original.events[..=turn_end].iter().map(|e| e.id.as_str()).collect();
    want.extend(l.events[original.events.len()..].iter().map(|e| e.id.as_str()));
    assert_eq!(active, want);
    assert!(want.contains(&a.as_str()));
    assert!(original.events.len() > turn_end + 20, "the abandoned branch is the whole rewound tail");
}

/// Opposes the test above: parallel tool calls (each result's `parentUuid` is
/// its own `tool_use` line, not the previous line) and progress lines are no
/// branch. The 2.1.11 fixture has three parallel triples, the 2.1.19 subagent
/// thirty-three such jumps and a tool result after a run of progress lines.
#[test]
fn parallel_tool_results_and_progress_lines_are_no_branch() {
    for rel in ["claude-code/v2.1.11-tool-results/native", "claude-code/v2.1.19-sidecar-progress/native", "claude-code/tool-calls/native", FLAT_SESSION] {
        let set = ClaudeCode.import(&fixture(rel), &ImportOptions::standard()).unwrap().set;
        for l in set.iter() {
            assert!(!l.is_branched(), "{rel}: {} is branched", l.id());
            assert_eq!(l.active_path().len(), l.events.len(), "{rel}: {}", l.id());
        }
    }
}

/// The `uuid` chain from the last line back through `parentUuid`.
fn active_chain(lines: &[serde_json::Value]) -> Vec<String> {
    let by: HashMap<&str, &serde_json::Value> = lines.iter().filter_map(|v| Some((v["uuid"].as_str()?, v))).collect();
    let mut out = vec![];
    let mut cur = lines.iter().rev().find_map(|v| v["uuid"].as_str());
    while let Some(u) = cur {
        out.push(u.to_string());
        cur = by.get(u).and_then(|v| v["parentUuid"].as_str());
    }
    out
}

/// (b) A rewind Claude Code wrote is written back as its own `parentUuid`
/// tree (SPEC §2.3 MAY: this exporter renders the abandoned branch of a
/// native rewind), byte for byte, with the abandoned lines off the active chain.
#[test]
fn a_native_rewind_is_written_back_with_the_abandoned_lines_off_the_active_chain() {
    let (_dir, path, [u, _]) = rewound_session();
    let st = crate::roundtrip::self_trip(&ClaudeCode, &path).unwrap();
    assert!(st.files.is_exact(), "self trip not exact: differ {:?}", st.files.differ);
    let out = ClaudeCode.export(&ClaudeCode.import(&path, &ImportOptions::standard()).unwrap().set, &ExportOptions::standard()).unwrap();
    let key = path.file_name().unwrap().to_str().unwrap();
    let lines = lines(&out.output, key);
    let chain = active_chain(&lines);
    let commit = lines.iter().find(|v| v["message"]["content"] == json!("commit")).unwrap()["uuid"].as_str().unwrap().to_string();
    assert!(chain.contains(&u) && chain.iter().any(|c| c.starts_with(TURN_END)));
    assert!(!chain.contains(&commit), "the rewound prompt is on the active chain");
    // Through the other primaries the abandoned lines ride in carriers and come back.
    for via in ["codex", "opencode"] {
        let ct = crate::roundtrip::cross_trip(&ClaudeCode, &path, crate::formats::by_name(via).unwrap(), true).unwrap();
        assert_eq!(ct.there.score(), 0, "carried trip via {via} lost something");
        assert!(ct.back_files.is_exact(), "carry trip via {via} not exact: differ {:?}", ct.back_files.differ);
    }
}

/// The text of every message block a transcript renders (carriers aside).
fn rendered_texts(lines: &[serde_json::Value]) -> Vec<String> {
    let mut out = vec![];
    for v in lines {
        match &v["message"]["content"] {
            serde_json::Value::String(t) => out.push(t.clone()),
            serde_json::Value::Array(bs) => {
                for b in bs {
                    for k in ["text", "name"] {
                        if let Some(t) = b[k].as_str() {
                            out.push(t.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// (b) A foreign branched ledger renders its active path only; with carry the
/// abandoned events ride along (and come back), and the resumed prompt hangs
/// off the line the branch continues from.
#[test]
fn a_foreign_branch_renders_only_its_active_path() {
    let src = fixture("protoledger/branch-rewind/native");
    let set = crate::ledger::LedgerSet::read_path(&src).unwrap();
    for opts in [ExportOptions::no_carry(), ExportOptions::standard()] {
        let out = ClaudeCode.export(&set, &opts).unwrap();
        let key = out.output.files.keys().next().unwrap().clone();
        let lines = lines(&out.output, &key);
        let texts = rendered_texts(&lines);
        assert_eq!(
            texts,
            [
                "Write a haiku about rain.",
                "Soft rain on the roof\ncounting every tired tile\nthe gutter hums low",
                "Never mind the count. Translate the haiku into French.",
                "Douce pluie sur le toit\ncomptant chaque tuile lasse\nla gouttière chante"
            ]
        );
        assert_eq!(out.losses.0.iter().find(|l| l.code == "abandoned-branch-not-rendered").map(|l| l.count), Some(4));
        assert_eq!(active_chain(&lines).len(), lines.iter().filter(|v| v["uuid"].is_string()).count(), "every rendered line is on the chain");
        if opts.carry {
            let back = reimport(&out.output, &ImportOptions::standard()).set;
            assert_eq!(back.iter().next().unwrap().events, set.iter().next().unwrap().events, "carried back exactly, branch and all");
        }
    }
}

/// A line whose `parentUuid` passes over only bookkeeping lines (here an
/// `attachment` between a user text+image line and the answer, in the real
/// transession 2.1.234 transcript) is no branch: no conversation line was
/// passed over, and the user line's image part stays on the active path.
#[test]
fn a_parent_past_an_attachment_line_is_no_branch() {
    let rel = "claude-code/transession-2.1.234/native";
    let set = ClaudeCode.import(&fixture(rel), &ImportOptions::standard()).unwrap().set;
    let l = set.iter().find(|l| l.events.iter().any(|e| e.id.starts_with("ac86a252"))).unwrap();
    let answer = l.events.iter().find(|e| e.id.starts_with("ac86a252")).unwrap();
    assert_eq!(answer.after, None, "an attachment line in between is no rewind");
    assert!(!l.is_branched());
    let image = l.events.iter().find(|e| e.id.starts_with("56dfdb8d") && e.kind == Kind::Media).expect("the user line's image part");
    assert!(l.active_path().iter().any(|e| e.id == image.id), "the image left the active path");
}

/// A message of reasoning alone keeps its usage through
/// import → export → import. Two sources: a foreign ledger (opencode writes
/// such steps), and a Claude Code line holding only reasoning with
/// `stop_reason: max_tokens`, rendered again. Each comes back with its usage exactly once, with
/// the same counts, keyed by the message id, and the export names no loss
/// of usage but the `stop-reason-synthesised` each writes (its line is
/// rendered), classed under usage, once.
#[test]
fn usage_of_a_reasoning_only_message_survives_the_trip() {
    let usage = json!({"model": "m", "input": 10, "output": 20, "cache_read": 3, "cache_write": 4});
    let set = foreign(&[
        (Kind::Text, json!({"role": "user", "text": "go"}), None),
        (Kind::Reasoning, json!({"text": "thinking"}), Some("r1")),
        (Kind::Usage, usage.clone(), Some("r1")),
    ]);
    let (ex, back) = plain_trip(&set);
    let got: Vec<Event> = events_of(&back).into_iter().filter(|e| e.kind == Kind::Usage).collect();
    assert_eq!(got.len(), 1, "foreign: {:?}", events_of(&back).iter().map(|e| (e.kind, &e.id)).collect::<Vec<_>>());
    for k in ["input", "output", "cache_read", "cache_write"] {
        assert_eq!(got[0].body[k], usage[k], "foreign: {k}");
    }
    let mid = got[0].group.clone().expect("a message id");
    assert_eq!(got[0].id, format!("{mid}#usage"), "keyed by the message id");
    // No usage is lost. The one loss of kind usage is the stop reason the
    // line is written with so that the usage settles there (classed under
    // usage), named once.
    assert_eq!(
        ex.losses.0.iter().filter(|l| l.kind == Some(Kind::Usage)).map(|l| (l.code.as_str(), l.count)).collect::<Vec<_>>(),
        [("stop-reason-synthesised", 1)],
        "{:?}",
        ex.losses.0
    );

    // The same from Claude Code's own lines: one thinking line whose stop
    // reason says the message ended on it (`max_tokens`), then a prompt.
    let line = |uuid: &str, parent: Option<&str>, v: serde_json::Value| {
        let mut m =
            json!({"parentUuid": parent, "isSidechain": false, "uuid": uuid, "timestamp": "2026-01-01T00:00:01.000Z", "sessionId": "s", "version": "2.1.280"});
        m.as_object_mut().unwrap().extend(v.as_object().unwrap().clone());
        m.to_string()
    };
    let text = [
        line("u0", None, json!({"type": "user", "message": {"role": "user", "content": "q"}})),
        line("a1", Some("u0"), json!({"type": "assistant", "message": {"id": "msg_r", "model": "claude-x", "role": "assistant", "stop_reason": "max_tokens",
            "content": [{"type": "thinking", "thinking": "hmm", "signature": "sig"}], "usage": {"input_tokens": 7, "output_tokens": 70, "cache_read_input_tokens": 5}}})),
        line("u1", Some("a1"), json!({"type": "user", "message": {"role": "user", "content": "again"}})),
    ]
    .join("\n")
        + "\n";
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("s.jsonl"), &text).unwrap();
    let first = ClaudeCode.import(&dir.path().join("s.jsonl"), &ImportOptions::standard()).unwrap().set;
    // Without their native records, so the exporter renders the lines
    // rather than writing the source's own lines back unchanged.
    let mut rendered = LedgerSet::new();
    for l in first.iter() {
        let mut l = l.clone();
        for e in &mut l.events {
            e.native = None;
        }
        rendered.insert(l);
    }
    let (ex, back) = plain_trip(&rendered);
    let line_of_r = all_lines(&ex.output).into_iter().find(|v| v["message"]["content"][0]["type"] == json!("thinking")).unwrap();
    assert_ne!(line_of_r["uuid"], json!("a1"), "the thinking line was written back, not rendered");
    for (what, set) in [("import", &first), ("import → export → import", &back)] {
        let u: Vec<Event> = events_of(set).into_iter().filter(|e| e.kind == Kind::Usage).collect();
        assert_eq!(u.len(), 1, "{what}");
        assert_eq!((u[0].body["input"].clone(), u[0].body["output"].clone(), u[0].body["cache_read"].clone()), (json!(7), json!(70), json!(5)), "{what}");
    }
    assert_eq!(events_of(&first).iter().find(|e| e.kind == Kind::Usage).unwrap().id, "msg_r#usage");
    // Rendered (not written back), the line is written with the stop reason
    // too: the one loss of kind usage, named once.
    assert_eq!(
        ex.losses.0.iter().filter(|l| l.kind == Some(Kind::Usage)).map(|l| (l.code.as_str(), l.count)).collect::<Vec<_>>(),
        [("stop-reason-synthesised", 1)],
        "{:?}",
        ex.losses.0
    );
}

/// Opposed: a message of reasoning and text gets its usage exactly once,
/// whichever of its lines carry it. Claude Code 2.1.2xx writes the final
/// counts and the stop reason on every line of the message (the thinking
/// line too); older versions write partial counts and no stop reason on the
/// thinking line. Either way the text line settles it (a reasoning line with
/// `end_turn` is followed by a reply), and the count is one through the trip
/// too.
#[test]
fn usage_of_a_reasoning_and_text_message_is_counted_once() {
    let a = |uuid: &str, parent: &str, block: serde_json::Value, stop: Option<&str>, out: u64| {
        json!({"parentUuid": parent, "isSidechain": false, "type": "assistant", "uuid": uuid, "timestamp": "2026-01-01T00:00:01.000Z",
               "sessionId": "s", "message": {"id": "msg_1", "model": "claude-x", "role": "assistant", "content": [block], "stop_reason": stop,
               "usage": {"input_tokens": 5, "output_tokens": out}}})
        .to_string()
    };
    let think = json!({"type": "thinking", "thinking": "", "signature": "sig"});
    let reply = json!({"type": "text", "text": "ans"});
    for (layout, stop, first_out) in [("2.1.2xx", Some("end_turn"), 40), ("older", None, 3)] {
        let text = [
            json!({"parentUuid": null, "type": "user", "uuid": "u0", "sessionId": "s", "message": {"role": "user", "content": "q"}}).to_string(),
            a("a1", "u0", think.clone(), stop, first_out),
            a("a2", "a1", reply.clone(), stop, 40),
        ]
        .join("\n")
            + "\n";
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("s.jsonl"), &text).unwrap();
        let first = ClaudeCode.import(&dir.path().join("s.jsonl"), &ImportOptions::standard()).unwrap().set;
        let (_, back) = plain_trip(&first);
        for (what, set) in [("import", &first), ("trip", &back)] {
            let u: Vec<Event> = events_of(set).into_iter().filter(|e| e.kind == Kind::Usage).collect();
            assert_eq!(u.len(), 1, "{layout} {what}: usage events {:?}", u.iter().map(|e| &e.id).collect::<Vec<_>>());
            assert_eq!(u[0].body["output"], json!(40), "{layout} {what}: the final counts");
        }
        let u = events_of(&first).into_iter().find(|e| e.kind == Kind::Usage).unwrap();
        assert_eq!(u.native.as_ref().unwrap().id.as_deref(), Some("a2"), "{layout}: settled on the text line");
    }
}

/// A subagent `start` carries the time of the call that spawned it, also
/// when it is written on a later line (an older layout names the agent only in
/// the tool result's `toolUseResult.agentId`, SPEC §9.3). Over every fixture;
/// the late-written population is named and must not be empty, and the
/// at-the-call population is its counterpart.
#[test]
fn a_subagent_start_carries_its_calls_time_wherever_it_is_written() {
    let (mut late, mut at_call) = (Vec::new(), 0usize);
    let mut cases: Vec<_> = std::fs::read_dir(fixture("claude-code")).unwrap().map(|e| e.unwrap().path()).collect();
    cases.sort();
    for case in cases {
        let name = case.file_name().unwrap().to_string_lossy().to_string();
        if name.starts_with(['_', '.']) || !case.join("native").is_dir() {
            continue;
        }
        let Ok(imp) = ClaudeCode.import(&case.join("native"), &ImportOptions::standard()) else { continue };
        for l in imp.set.iter() {
            for (i, e) in l.events.iter().enumerate() {
                if e.kind != Kind::Subagent || e.str("phase") != Some("start") {
                    continue;
                }
                let call = e.str("call_id").unwrap();
                let Some(c) = l.events.iter().position(|x| x.kind == Kind::ToolCall && x.str("call_id") == Some(call)) else { continue };
                assert_eq!(e.ts, l.events[c].ts, "{name}: the start of {call} is not stamped with its call's time");
                if l.events[c + 1..i].iter().any(|x| x.kind == Kind::ToolResult && x.str("call_id") == Some(call)) {
                    late.push(format!("{name}/{call}"));
                } else {
                    at_call += 1;
                }
            }
        }
    }
    eprintln!("subagent starts written after their call's result: {late:?}; at the call: {at_call}");
    assert!(late.iter().any(|x| x.starts_with("v2.1.19-sidecar-progress/")), "{late:?}");
    assert!(at_call > 0);
}

/// Writing a stop reason the ledger did not hold is a named loss,
/// `stop-reason-synthesised`, raised exactly where the exporter writes one.
/// Fires on a foreign message of reasoning alone (its last line is written
/// with `stop_reason: max_tokens`), with carry and without. Does not fire on a
/// foreign message of reasoning and text (its usage is rendered too, on the
/// text line, with no stop reason written), nor on a Claude Code message whose
/// own line states `max_tokens` (written back as it was: the output holds the
/// same stop reason, and nothing was synthesised).
#[test]
fn a_stop_reason_the_ledger_did_not_hold_is_named_stop_reason_synthesised() {
    let synthesised = |ex: &crate::formats::Exported| -> u64 { ex.losses.0.iter().filter(|l| l.code == "stop-reason-synthesised").map(|l| l.count).sum() };
    let stops = |ex: &crate::formats::Exported| -> Vec<serde_json::Value> {
        all_lines(&ex.output).into_iter().filter(|v| v["type"] == json!("assistant")).map(|v| v["message"]["stop_reason"].clone()).collect()
    };
    let usage = json!({"model": "m", "input": 10, "output": 20});

    // Fires: a foreign message of reasoning alone.
    let set = foreign(&[
        (Kind::Text, json!({"role": "user", "text": "go"}), None),
        (Kind::Reasoning, json!({"text": "thinking"}), Some("r1")),
        (Kind::Usage, usage.clone(), Some("r1")),
    ]);
    for opts in [ExportOptions::no_carry(), ExportOptions::standard()] {
        let ex = ClaudeCode.export(&set, &opts).unwrap();
        assert_eq!(stops(&ex), [json!("max_tokens")], "the reasoning line is written with max_tokens");
        assert_eq!(synthesised(&ex), 1, "foreign reasoning alone: {:?}", ex.losses.0);
    }

    // Does not fire: a foreign message of reasoning and text. Its usage is
    // rendered (on the text line), and no stop reason is written.
    let set = foreign(&[
        (Kind::Text, json!({"role": "user", "text": "go"}), None),
        (Kind::Reasoning, json!({"text": "thinking"}), Some("r1")),
        (Kind::Text, json!({"role": "assistant", "text": "done"}), Some("r1")),
        (Kind::Usage, usage.clone(), Some("r1")),
    ]);
    let ex = ClaudeCode.export(&set, &ExportOptions::no_carry()).unwrap();
    let rendered = all_lines(&ex.output);
    assert!(rendered.iter().any(|v| v["message"]["usage"]["output_tokens"] == json!(20)), "the usage was rendered: {rendered:?}");
    assert_eq!(stops(&ex), [serde_json::Value::Null, serde_json::Value::Null], "no stop reason written");
    assert_eq!(synthesised(&ex), 0, "foreign reasoning and text: {:?}", ex.losses.0);

    // Does not fire: a Claude Code message of reasoning alone that states
    // its own `stop_reason: max_tokens`, written back as it was.
    let text = [
        json!({"parentUuid": null, "isSidechain": false, "type": "user", "uuid": "u0", "timestamp": "2026-01-01T00:00:00.000Z", "sessionId": "s",
               "message": {"role": "user", "content": "q"}}),
        json!({"parentUuid": "u0", "isSidechain": false, "type": "assistant", "uuid": "a1", "timestamp": "2026-01-01T00:00:01.000Z", "sessionId": "s",
               "message": {"id": "msg_r", "model": "claude-x", "role": "assistant", "stop_reason": "max_tokens",
                           "content": [{"type": "thinking", "thinking": "hmm", "signature": "sig"}], "usage": {"input_tokens": 7, "output_tokens": 70}}}),
    ]
    .iter()
    .map(|v| v.to_string() + "\n")
    .collect::<String>();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("s.jsonl"), &text).unwrap();
    let native = ClaudeCode.import(&dir.path().join("s.jsonl"), &ImportOptions::standard()).unwrap().set;
    assert!(events_of(&native).iter().any(|e| e.kind == Kind::Usage), "the native message has its usage");
    for opts in [ExportOptions::no_carry(), ExportOptions::standard()] {
        let ex = ClaudeCode.export(&native, &opts).unwrap();
        assert_eq!(ex.output.files["s.jsonl"], text.as_bytes(), "written back as it was");
        assert_eq!(stops(&ex), [json!("max_tokens")], "the output states max_tokens, from the source");
        assert_eq!(synthesised(&ex), 0, "Claude Code's own stop reason: {:?}", ex.losses.0);
    }
}

/// The exporter leaves a line already written with
/// `stop_reason: max_tokens` as it is, and does not name it again. A line
/// reaches the exporter's usage rendering already holding `max_tokens` only
/// when a foreign message of reasoning alone has a second usage (the first
/// wrote it; a Claude Code line is written back by `native_record` and never
/// gets there, which is why the native case above cannot reach the guard).
/// With the guard, that message exports exactly as it does with one usage,
/// and `stop-reason-synthesised` is named once, for the one line it wrote;
/// without it, the one line is named twice. Opposed: a message whose line
/// holds no stop reason is written with `max_tokens` and the loss is named
/// (an exporter that never synthesises fails), and the loss is classed under
/// `Kind::Usage`: the stop reason is where the importer settles the usage.
#[test]
fn a_line_already_written_with_max_tokens_is_left_alone_and_named_once_under_usage() {
    let synthesised = |ex: &crate::formats::Exported| -> Vec<(Option<Kind>, u64)> {
        ex.losses.0.iter().filter(|l| l.code == "stop-reason-synthesised").map(|l| (l.kind, l.count)).collect()
    };
    let stops = |ex: &crate::formats::Exported| -> Vec<serde_json::Value> {
        all_lines(&ex.output).into_iter().filter(|v| v["type"] == json!("assistant")).map(|v| v["message"]["stop_reason"].clone()).collect()
    };
    let usage = json!({"model": "m", "input": 10, "output": 20});
    let head = [(Kind::Text, json!({"role": "user", "text": "go"}), None), (Kind::Reasoning, json!({"text": "thinking"}), Some("r1"))];
    let once = foreign(&[head[0].clone(), head[1].clone(), (Kind::Usage, usage.clone(), Some("r1"))]);
    let twice = foreign(&[head[0].clone(), head[1].clone(), (Kind::Usage, usage.clone(), Some("r1")), (Kind::Usage, usage.clone(), Some("r1"))]);
    for opts in [ExportOptions::no_carry(), ExportOptions::standard()] {
        // Absent: synthesised, and named, under usage.
        let one = ClaudeCode.export(&once, &opts).unwrap();
        assert_eq!(stops(&one), [json!("max_tokens")], "carry {}: the reasoning line is written with max_tokens", opts.carry);
        assert_eq!(synthesised(&one), [(Some(Kind::Usage), 1)], "carry {}: one usage: {:?}", opts.carry, one.losses.0);
        // Already max_tokens when the second usage reaches the line: left alone.
        let two = ClaudeCode.export(&twice, &opts).unwrap();
        assert_eq!(stops(&two), [json!("max_tokens")], "carry {}: the line still states max_tokens", opts.carry);
        assert_eq!(
            synthesised(&two),
            [(Some(Kind::Usage), 1)],
            "carry {}: a line already written with max_tokens was named again: {:?}",
            opts.carry,
            two.losses.0
        );
        if !opts.carry {
            assert!(two.output == one.output, "without carry, the second usage changed the export");
        }
    }
}
