//! Unit tests for the Codex adapter.
//!
//! `foreign_export_follows_codex_loader_rules` mirrors, as far as they can be
//! read from Codex's source, the rules Codex applies when it lists, loads and
//! resumes a rollout. The same exports were also loaded with Codex's own
//! crates (see `docs/formats/codex.md`, "Verification"); set
//! `PROTOLEDGER_CODEX_EXPORT_DIR` to write them out for that check.

use super::*;
use crate::formats::{ExportOptions, ImportOptions};
use crate::ledger::Ledger;
use crate::loss::{CompareOptions, compare};
use crate::model::{Kind, Link, Obj, Parent, Relation, Source};
use serde_json::json;

fn ev(id: &str, kind: Kind, ts: &str) -> Event {
    Event::new(id, kind).ts(Some(ts))
}

/// A thread as another harness would hand it over: a parent with a native
/// subagent, reasoning sealed by another vendor, tools, usage, compaction,
/// media, a title and a peer message.
fn foreign_set() -> LedgerSet {
    let mut root = Ledger::new(Agent {
        id: "sess-root".into(),
        created_at: Some("2026-10-01T10:00:00.000Z".into()),
        source: Some(Source {
            format: "claude-code".into(),
            variant: Some("jsonl".into()),
            version: Some("2.1.0".into()),
            native_id: Some("sess-root".into()),
            extra: Obj::new(),
        }),
        parent: None,
        links: vec![Link { rel: "member-of".into(), agent: Some("net".into()), event: None, extra: Obj::new() }],
        meta: {
            let mut m = Obj::new();
            m.insert("example.app".into(), json!({"room": "r1"}));
            m
        },
    });
    let evs = vec![
        ev("s1", Kind::Settings, "2026-10-01T10:00:00.000Z")
            .with("cwd", "/home/user/project")
            .with("model", "claude-opus")
            .with("provider", "anthropic")
            .with("git_branch", "main")
            .with("system_prompt", "You are a helpful agent."),
        ev("t0", Kind::Title, "2026-10-01T10:00:00.500Z").with("text", "Fix the parser"),
        ev("u1", Kind::Text, "2026-10-01T10:00:01.000Z").with("role", "user").with("text", "Fix the parser bug").group(Some("m1")),
        ev("u1img", Kind::Media, "2026-10-01T10:00:01.000Z")
            .with("role", "user")
            .with("media_type", "image/png")
            .with("data", "iVBORw0KGgo=")
            .group(Some("m1")),
        ev("r1", Kind::Reasoning, "2026-10-01T10:00:02.000Z")
            .group(Some("msg_resp2"))
            .with("text", "Look at parse.rs first.")
            .with("summary", json!(["Inspecting the parser"]))
            .with("sealed", json!({"vendor": "anthropic", "kind": "signature", "data": "sig=="})),
        ev("a1", Kind::Text, "2026-10-01T10:00:03.000Z").group(Some("msg_resp2")).with("role", "assistant").with("text", "Reading the file."),
        ev("c1", Kind::ToolCall, "2026-10-01T10:00:04.000Z")
            .group(Some("msg_resp2"))
            .with("call_id", "toolu_1")
            .with("name", "Read")
            .with("input", json!({"file_path": "src/parse.rs"}))
            .with("tool_type", "function"),
        ev("o1", Kind::ToolResult, "2026-10-01T10:00:05.000Z").with("call_id", "toolu_1").with("output", "fn parse() {}"),
        ev("c2", Kind::ToolCall, "2026-10-01T10:00:06.000Z").with("call_id", "toolu_2").with("name", "Task").with("input", json!({"prompt": "check tests"})),
        ev("sa", Kind::Subagent, "2026-10-01T10:00:06.500Z").with("phase", "start").with("agent", "sess-child").with("call_id", "toolu_2"),
        ev("o2", Kind::ToolResult, "2026-10-01T10:00:09.000Z").with("call_id", "toolu_2").with("output", json!([{"type": "text", "text": "tests pass"}])),
        ev("c3", Kind::ToolCall, "2026-10-01T10:00:10.000Z")
            .with("call_id", "toolu_3")
            .with("name", "apply_patch")
            .with("input_text", "*** Begin Patch\n*** End Patch")
            .with("tool_type", "custom"),
        ev("o3", Kind::ToolResult, "2026-10-01T10:00:11.000Z").with("call_id", "toolu_3").with("output", "Done").with("is_error", false),
        ev("us1", Kind::Usage, "2026-10-01T10:00:11.500Z").group(Some("msg_resp2")).with("input", 1200).with("output", 80).with("cache_read", 1000),
        ev("a2", Kind::Text, "2026-10-01T10:00:12.000Z").group(Some("step-7")).with("role", "assistant").with("text", "Fixed."),
        ev("pm", Kind::PeerMessage, "2026-10-01T10:00:12.500Z").with("direction", "in").with("peer", "reviewer").with("text", "ship it"),
        ev("cb", Kind::Compaction, "2026-10-01T10:00:13.000Z").with("phase", "boundary").with("trigger", "auto"),
        ev("cs", Kind::Compaction, "2026-10-01T10:00:13.100Z").with("phase", "summary").with("summary", "We fixed the parser."),
        ev("u2", Kind::Text, "2026-10-01T10:00:14.000Z").with("role", "user").with("text", "Now add a test"),
        ev("c4", Kind::ToolCall, "2026-10-01T10:00:15.000Z").with("call_id", "toolu_4").with("name", "Bash").with("input", json!({"command": "cargo test"})),
        ev("fc", Kind::FileChange, "2026-10-01T10:00:16.000Z").with("path", "src/parse.rs").with("op", "edit"),
    ];
    for e in evs {
        root.append(e).unwrap();
    }
    let mut child = Ledger::new(Agent {
        id: "sess-child".into(),
        created_at: Some("2026-10-01T10:00:06.600Z".into()),
        source: None,
        parent: Some(Parent { agent: "sess-root".into(), event: Some("sa".into()), relation: Relation::Subagent }),
        links: vec![],
        meta: Obj::new(),
    });
    for e in [
        ev("k1", Kind::Text, "2026-10-01T10:00:06.700Z").with("role", "user").with("text", "check tests"),
        ev("k2", Kind::Text, "2026-10-01T10:00:08.000Z").with("role", "assistant").with("text", "tests pass"),
    ] {
        child.append(e).unwrap();
    }
    let mut set = LedgerSet::new();
    set.insert(root);
    set.insert(child);
    set
}

fn export_to_dir(set: &LedgerSet, opts: &ExportOptions) -> (tempfile::TempDir, crate::formats::Exported) {
    let out = Codex.export(set, opts).unwrap();
    let dir = tempfile::tempdir().unwrap();
    out.output.write_to(dir.path(), true).unwrap();
    if let Ok(d) = std::env::var("PROTOLEDGER_CODEX_EXPORT_DIR") {
        let v = opts.variant.clone().unwrap_or_else(|| "legacy".into());
        let tag = if opts.carry { "carry" } else { "plain" };
        out.output.write_to(&Path::new(&d).join(format!("{v}-{tag}")), true).unwrap();
    }
    (dir, out)
}

#[test]
fn foreign_carry_round_trip_is_exact() {
    let set = foreign_set();
    for variant in [None, Some("paginated"), Some("rollout-v0"), Some("ts-json")] {
        let opts = ExportOptions { variant: variant.map(str::to_string), ..ExportOptions::standard() };
        let (dir, _) = export_to_dir(&set, &opts);
        let back = Codex.import(dir.path(), &ImportOptions::standard()).unwrap().set;
        let d = compare(&set, &back, CompareOptions::default());
        assert!(d.is_clean(), "{variant:?}: {}", d.summary());
        for l in set.iter() {
            assert_eq!(back.get(l.id()).map(|b| &b.events), Some(&l.events), "{variant:?} {}", l.id());
            assert_eq!(back.get(l.id()).map(|b| b.agent()), Some(l.agent()), "{variant:?} header");
        }
        // And exporting what came back reproduces the files byte for byte.
        let again = Codex.export(&back, &opts).unwrap();
        let first = Codex.export(&set, &opts).unwrap();
        assert_eq!(again.output, first.output, "{variant:?}");
    }
}

#[test]
fn foreign_plain_export_keeps_the_conversation() {
    let set = foreign_set();
    let (dir, out) = export_to_dir(&set, &ExportOptions::no_carry());
    let codes: Vec<&str> = out.losses.0.iter().map(|l| l.code.as_str()).collect();
    for c in ["reasoning-sealed-dropped", "file-change-dropped", "settings-not-rendered"] {
        assert!(codes.contains(&c), "missing loss {c}: {codes:?}");
    }
    let back = Codex.import(dir.path(), &ImportOptions::standard()).unwrap().set;
    let root = back.iter().find(|l| l.agent().parent.is_none()).unwrap();
    let texts: Vec<(&str, &str)> = root.events.iter().filter(|e| e.kind == Kind::Text).map(|e| (e.str("role").unwrap(), e.str("text").unwrap())).collect();
    assert_eq!(texts, vec![("user", "Fix the parser bug"), ("assistant", "Reading the file."), ("assistant", "Fixed."), ("user", "Now add a test")]);
    // The UI copies came back as native events, not as second texts.
    assert!(root.events.iter().any(|e| e.kind == Kind::Native && e.meta.contains_key("codex")));
    let calls: Vec<&str> = root.events.iter().filter(|e| e.kind == Kind::ToolCall).map(|e| e.str("call_id").unwrap()).collect();
    let results: Vec<&str> = root.events.iter().filter(|e| e.kind == Kind::ToolResult).map(|e| e.str("call_id").unwrap()).collect();
    // The Bash call left without a result stays so; Codex adds `aborted` itself.
    assert_eq!(calls[..calls.len() - 1], results[..], "every other call is paired");
    let child = back.iter().find(|l| l.agent().parent.is_some()).unwrap();
    assert_eq!(child.agent().parent.as_ref().unwrap().agent, root.id());
    assert_eq!(child.agent().parent.as_ref().unwrap().relation, Relation::Subagent);
}

/// The rules a Codex loader applies, each cited.
fn check_codex_rules(dir: &Path, paginated: bool) -> Vec<String> {
    let mut problems = Vec::new();
    for (rel, path) in rollout_files(dir).unwrap() {
        // rollout_file_name.rs:44-70: sessions/YYYY/MM/DD/rollout-YYYY-MM-DDThh-mm-ss-<uuid>.jsonl
        let name = rel.rsplit('/').next().unwrap();
        let parts: Vec<&str> = rel.split('/').collect();
        let core = name.strip_prefix("rollout-").and_then(|c| c.strip_suffix(".jsonl"));
        let ok_name = parts.len() == 5
            && parts[0] == "sessions"
            && core.is_some_and(|c| c.len() > 20 && &c[19..20] == "-" && uuid::Uuid::parse_str(&c[20..]).is_ok())
            && core.is_some_and(|c| c[..10] == format!("{}-{}-{}", parts[1], parts[2], parts[3]));
        if !ok_name {
            problems.push(format!("{rel}: file name does not parse"));
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let mut thread = None;
        let mut calls = Vec::new();
        let mut function_calls: Vec<String> = Vec::new();
        let mut outputs = Vec::new();
        let mut ordinal = 0u64;
        for (i, line) in text.lines().enumerate() {
            let v: Value = serde_json::from_str(line).unwrap();
            // lib.rs:52-75: timestamp required, ordinal optional, rest is RolloutItemWire.
            if !v.get("timestamp").is_some_and(Value::is_string) {
                problems.push(format!("{rel}:{}: no timestamp", i + 1));
            }
            if paginated {
                // ordinal.rs:20-44: every paginated record carries the next ordinal.
                if v.get("ordinal").and_then(Value::as_u64) != Some(ordinal) {
                    problems.push(format!("{rel}:{}: ordinal {:?} where {ordinal} was expected", i + 1, v.get("ordinal")));
                }
                ordinal += 1;
            }
            let ty = v["type"].as_str().unwrap_or("");
            let p = &v["payload"];
            // client.rs:1098-1103: a prefixed item id is sent to the Responses API,
            // which expects its own prefix per item type; anything else is stripped.
            if let Some(id) = p.get("id").and_then(Value::as_str).filter(|_| ty == "response_item") {
                let want = match p["type"].as_str().unwrap_or("") {
                    "message" => "msg",
                    "function_call" => "fc",
                    "custom_tool_call" => "ctc",
                    _ => "",
                };
                if super::import::is_api_item_id(id) && !id.starts_with(&format!("{want}_")) {
                    problems.push(format!("{rel}: item id {id} would be sent to the API with the wrong prefix"));
                }
            }
            match ty {
                "session_meta" => {
                    // recorder.rs:1131-1135: the first SessionMeta names the thread.
                    if i != 0 {
                        problems.push(format!("{rel}: session_meta is not first"));
                    }
                    let id = p["id"].as_str().unwrap_or("");
                    if uuid::Uuid::parse_str(id).is_err() {
                        problems.push(format!("{rel}: thread id {id:?} is not a UUID"));
                    }
                    // model_provider is an id from the user's config, or null (list.rs:958-963).
                    if !p["model_provider"].is_null() && p["model_provider"] != "openai" {
                        problems.push(format!("{rel}: model_provider {} is not a Codex provider id", p["model_provider"]));
                    }
                    // protocol.rs:3254-3330: required fields.
                    for k in ["timestamp", "cwd", "originator", "cli_version"] {
                        if !p.get(k).is_some_and(Value::is_string) {
                            problems.push(format!("{rel}: session_meta.{k} missing"));
                        }
                    }
                    // recorder.rs:1240-1253: history_mode must be a known mode.
                    let hm = p["history_mode"].as_str().unwrap_or("legacy");
                    if !["legacy", "paginated"].contains(&hm) || (hm == "paginated") != paginated {
                        problems.push(format!("{rel}: history_mode {hm}"));
                    }
                    thread = Some(id.to_string());
                }
                "response_item" => match p["type"].as_str().unwrap_or("") {
                    "function_call" => function_calls.push(p["call_id"].as_str().unwrap_or("").to_string()),
                    "custom_tool_call" => calls.push(p["call_id"].as_str().unwrap_or("").to_string()),
                    "function_call_output" | "custom_tool_call_output" => outputs.push(p["call_id"].as_str().unwrap_or("").to_string()),
                    "reasoning" => {
                        // models.rs:1101-1113: encrypted_content is present, and only
                        // ever real ciphertext (here: none was available).
                        if !p.get("encrypted_content").is_some_and(Value::is_null) {
                            problems.push(format!("{rel}: reasoning encrypted_content is not null"));
                        }
                        if !p["summary"].is_array() {
                            problems.push(format!("{rel}: reasoning summary missing"));
                        }
                    }
                    "message" => {
                        if !p["content"].is_array() || !p["role"].is_string() {
                            problems.push(format!("{rel}: malformed message"));
                        }
                    }
                    other => problems.push(format!("{rel}: unexpected response item {other}")),
                },
                "event_msg" => {
                    // EventMsg has no catch-all variant (protocol.rs:1469): only known types.
                    let et = p["type"].as_str().unwrap_or("");
                    let known = [
                        "task_started",
                        "task_complete",
                        "turn_aborted",
                        "user_message",
                        "agent_message",
                        "agent_reasoning",
                        "agent_reasoning_raw_content",
                        "token_count",
                        "context_compacted",
                        "item_completed",
                        "patch_apply_end",
                    ];
                    if !known.contains(&et) {
                        problems.push(format!("{rel}: event_msg {et}"));
                    }
                    // policy.rs:120-140: legacy-only UI events never in paginated files.
                    if paginated && ["user_message", "agent_message", "agent_reasoning", "patch_apply_end"].contains(&et) {
                        problems.push(format!("{rel}: legacy UI event {et} in a paginated file"));
                    }
                    if et == "item_completed" && p["thread_id"].as_str() != thread.as_deref() {
                        problems.push(format!("{rel}: item_completed for another thread"));
                    }
                }
                "token_usage_record" => {
                    // protocol.rs:2389-2400: every field is required.
                    for k in ["thread_id", "turn_id", "session_id", "root_turn_id", "response_id"] {
                        if !p[k].is_string() {
                            problems.push(format!("{rel}: token_usage_record.{k} missing"));
                        }
                    }
                    if p["thread_id"].as_str() != thread.as_deref() {
                        problems.push(format!("{rel}: token_usage_record for another thread"));
                    }
                    for k in ["usage", "turn_token_usage", "thread_token_usage"] {
                        for f in ["input_tokens", "cached_input_tokens", "output_tokens", "reasoning_output_tokens", "total_tokens"] {
                            if !p[k][f].is_i64() {
                                problems.push(format!("{rel}: token_usage_record.{k}.{f} missing"));
                            }
                        }
                    }
                }
                "compacted" => {
                    if !p["message"].is_string() {
                        problems.push(format!("{rel}: compacted.message missing"));
                    }
                }
                "inter_agent_communication" => {
                    // agent_path.rs:149-170: absolute paths start with /root.
                    for k in ["author", "recipient"] {
                        if !p[k].as_str().is_some_and(|s| s == "/root" || s.starts_with("/root/")) {
                            problems.push(format!("{rel}: {k} is not an agent path"));
                        }
                    }
                }
                other => problems.push(format!("{rel}: unexpected line type {other}")),
            }
        }
        // normalize.rs:21-130, 171-210: a custom tool call has exactly one output (a
        // missing one is error_or_panic); a function call may have none (Codex adds
        // `aborted`); no output is an orphan.
        let mut o = outputs.clone();
        o.retain(|x| !function_calls.contains(x));
        let mut c = calls.clone();
        c.sort();
        o.sort();
        if c != o {
            problems.push(format!("{rel}: custom calls {calls:?} vs outputs {outputs:?}"));
        }
        let mut seen = outputs.clone();
        seen.sort();
        seen.dedup();
        if seen.len() != outputs.len() {
            problems.push(format!("{rel}: a call has two outputs"));
        }
        if thread.is_none() {
            problems.push(format!("{rel}: no session_meta"));
        }
    }
    problems
}

#[test]
fn foreign_export_follows_codex_loader_rules() {
    let set = foreign_set();
    for (variant, paginated) in [(None, false), (Some("paginated"), true)] {
        for carry in [false, true] {
            let opts = ExportOptions { variant: variant.map(str::to_string), carry, target_version: None };
            let (dir, out) = export_to_dir(&set, &opts);
            assert_eq!(out.output.files.len(), 3, "two rollouts and the session index");
            let mut problems = check_codex_rules(dir.path(), paginated);
            // session_index.rs:24-29: {id, thread_name, updated_at}, the id a thread of the set.
            let index = std::fs::read_to_string(dir.path().join(SESSION_INDEX)).unwrap();
            let threads: Vec<String> = rollout_files(dir.path()).unwrap().iter().map(|(rel, _)| rel[rel.len() - 42..rel.len() - 6].to_string()).collect();
            for line in index.lines() {
                let v: Value = serde_json::from_str(line).unwrap();
                if !v["id"].as_str().is_some_and(|i| threads.iter().any(|t| t == i)) || !v["thread_name"].is_string() || !v["updated_at"].is_string() {
                    problems.push(format!("session_index line {line}"));
                }
            }
            assert!(problems.is_empty(), "{variant:?} carry={carry}: {problems:#?}");
        }
    }
}

#[test]
fn subagent_source_and_session_id_point_at_the_parent() {
    let set = foreign_set();
    let out = Codex.export(&set, &ExportOptions::no_carry()).unwrap();
    let metas: Vec<Value> =
        out.output.files.values().map(|b| serde_json::from_str::<Value>(std::str::from_utf8(b).unwrap().lines().next().unwrap()).unwrap()).collect();
    let root = metas.iter().find(|m| m["payload"]["source"] == "cli").unwrap();
    let child = metas.iter().find(|m| m["payload"]["source"].is_object()).unwrap();
    let spawn = &child["payload"]["source"]["subagent"]["thread_spawn"];
    assert_eq!(spawn["parent_thread_id"], root["payload"]["id"]);
    assert_eq!(spawn["depth"], 1);
    assert_eq!(child["payload"]["session_id"], root["payload"]["id"]);
    // The ledger names Anthropic, which Codex has no provider id for: left unset, not rewritten.
    assert_eq!(root["payload"]["model_provider"], Value::Null);
    assert_eq!(root["payload"]["base_instructions"]["text"], "You are a helpful agent.");
}

#[test]
fn token_seeding_matches_codex_importer() {
    // No usage in the ledger → one token_count with total = ceil(bytes / 4).
    let mut l = Ledger::new(Agent { id: "a".into(), created_at: None, source: None, parent: None, links: vec![], meta: Obj::new() });
    l.append(ev("u", Kind::Text, "2026-01-01T00:00:00Z").with("role", "user").with("text", "12345678")).unwrap();
    l.append(ev("a", Kind::Text, "2026-01-01T00:00:01Z").with("role", "assistant").with("text", "1234")).unwrap();
    let out = Codex.export(&LedgerSet::single(l), &ExportOptions::no_carry()).unwrap();
    let text = String::from_utf8(out.output.files.values().next().unwrap().clone()).unwrap();
    let tc: Vec<Value> = text.lines().map(|l| serde_json::from_str::<Value>(l).unwrap()).filter(|v| v["payload"]["type"] == "token_count").collect();
    assert_eq!(tc.len(), 1);
    assert_eq!(tc[0]["payload"]["info"]["total_token_usage"]["total_tokens"], 3);
    let types: Vec<String> = text
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .map(|v| format!("{}/{}", v["type"].as_str().unwrap(), v["payload"]["type"].as_str().unwrap_or("")))
        .collect();
    assert_eq!(
        types,
        [
            "session_meta/",
            "event_msg/task_started",
            "response_item/message",
            "event_msg/user_message",
            "response_item/message",
            "event_msg/agent_message",
            "event_msg/token_count",
            "event_msg/task_complete"
        ]
    );
}

#[test]
fn ui_copies_are_not_double_counted() {
    let text = concat!(
        r#"{"timestamp":"2026-01-01T00:00:00.000Z","type":"session_meta","payload":{"id":"0199a1b0-0000-7000-8000-000000000001","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/w","originator":"codex_cli_rs","cli_version":"0.50.0"}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:01.000Z","type":"event_msg","payload":{"type":"user_message","message":"hi","images":null}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:02.000Z","type":"event_msg","payload":{"type":"agent_reasoning","text":"thinking"}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:02.000Z","type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"thinking"}],"encrypted_content":"gAAAA"}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:03.000Z","type":"event_msg","payload":{"type":"agent_message","message":"only in the UI"}}"#,
        "\n",
    );
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("rollout-2026-01-01T00-00-00-0199a1b0-0000-7000-8000-000000000001.jsonl");
    std::fs::write(&p, text).unwrap();
    let imported = Codex.import(&p, &ImportOptions::standard()).unwrap();
    let set = imported.set;
    let l = set.iter().next().unwrap();
    let kinds: Vec<Kind> = l.events.iter().map(|e| e.kind).collect();
    // SPEC §9.3: each UI record is settled by what was read before it. The two
    // copies read before their items wait (`ui_copy_of` null) and their items
    // claim them; the last one's item may still be written, so it waits too.
    assert_eq!(kinds, [Kind::Settings, Kind::Native, Kind::Text, Kind::Native, Kind::Reasoning, Kind::Native]);
    for i in [1, 3, 5] {
        assert_eq!(l.events[i].meta["codex"]["ui_copy_of"], Value::Null, "event {i}");
    }
    let r = &l.events[4];
    assert_eq!(r.body["sealed"], json!({"vendor": "openai", "kind": "encrypted", "data": "gAAAA"}));
    assert!(imported.losses.0.iter().any(|x| x.code == "ui-record-unsettled" && x.count == 1), "{:?}", imported.losses);
    // And the self trip is exact.
    let out = Codex.export(&set, &ExportOptions::standard()).unwrap();
    assert_eq!(out.output.files.values().next().unwrap(), text.as_bytes());

    // The turn ends with no item for the last UI record: its content is
    // emitted there, once; a prompt recorded before its copy is named by it.
    let more = concat!(
        r#"{"timestamp":"2026-01-01T00:00:04.000Z","type":"event_msg","payload":{"type":"task_complete","turn_id":null}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:05.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"again"}]}}"#,
        "\n",
        r#"{"timestamp":"2026-01-01T00:00:05.000Z","type":"event_msg","payload":{"type":"user_message","message":"again","images":null}}"#,
        "\n",
    );
    let text2 = format!("{text}{more}");
    std::fs::write(&p, &text2).unwrap();
    let imported = Codex.import(&p, &ImportOptions::standard()).unwrap();
    assert!(!imported.losses.0.iter().any(|x| x.code == "ui-record-unsettled"), "{:?}", imported.losses);
    let l2 = imported.set.iter().next().unwrap();
    assert_eq!(l2.events[..6], l.events[..], "the earlier events are unchanged");
    let kinds: Vec<Kind> = l2.events.iter().map(|e| e.kind).collect();
    assert_eq!(kinds[6..], [Kind::Text, Kind::Turn, Kind::Text, Kind::Native]);
    let shown = &l2.events[6];
    assert_eq!((shown.str("role"), shown.str("text")), (Some("assistant"), Some("only in the UI")));
    assert_eq!(shown.id, format!("{}#1", l2.events[5].id));
    assert_eq!(shown.native.as_ref().map(|n| (n.id.clone(), n.part)), Some((l2.events[5].native.as_ref().unwrap().id.clone(), 1)));
    let texts: Vec<&str> = l2.events.iter().filter_map(|e| e.str("text")).collect();
    assert_eq!(texts, ["hi", "only in the UI", "again"], "every text once");
    assert_eq!(l2.events[9].meta["codex"]["ui_copy_of"], json!(l2.events[8].id));
    let out = Codex.export(&imported.set, &ExportOptions::standard()).unwrap();
    assert_eq!(out.output.files.values().next().unwrap(), text2.as_bytes());
}

#[test]
fn carrier_is_ignored_by_the_loader_shape_and_edits_import_fresh() {
    let set = foreign_set();
    let (dir, _) = export_to_dir(&set, &ExportOptions::standard());
    // Edit one carried line the way a user editing history in Codex would.
    let (_, path) = rollout_files(dir.path()).unwrap().into_iter().find(|(r, _)| r.contains(&ThreadTableTest::root(&set))).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let edited = text.replace("Fixed.", "Fixed it.");
    assert_ne!(text, edited);
    std::fs::write(&path, edited).unwrap();
    let back = Codex.import(dir.path(), &ImportOptions::standard()).unwrap().set;
    let root = back.get("sess-root").unwrap();
    assert!(root.events.iter().any(|e| e.str("text") == Some("Fixed it.")));
    assert!(!root.events.iter().any(|e| e.id == "a2"), "the edited event was imported fresh");
    assert!(root.events.iter().any(|e| e.id == "a1"), "unedited lines restore");
}

struct ThreadTableTest;
impl ThreadTableTest {
    fn root(_set: &LedgerSet) -> String {
        crate::util::uuid_for("sess-root")
    }
}

#[test]
#[cfg_attr(not(feature = "zstd"), ignore = "SKIPPED: needs the zstd feature")]
fn zstd_round_trip_and_variant_sniffing() {
    let text = "{\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"0199a1b0-0000-7000-8000-000000000001\",\"history_mode\":\"paginated\",\"cli_version\":\"0.150.0\"},\"ordinal\":0}\n";
    let z = zstd_encode(text.as_bytes()).unwrap();
    assert_eq!(zstd_decode(&z).unwrap(), text.as_bytes());
    variant_sniffing_holds(text);
}

/// The sniffing half needs no zstd, so a build without it still runs it.
#[test]
fn variant_sniffing() {
    let text = "{\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"0199a1b0-0000-7000-8000-000000000001\",\"history_mode\":\"paginated\",\"cli_version\":\"0.150.0\"},\"ordinal\":0}\n";
    variant_sniffing_holds(text);
}

fn variant_sniffing_holds(text: &str) {
    assert_eq!(sniff("x.jsonl", text).unwrap(), (V_PAGINATED, Some("0.150.0".into())));
    assert_eq!(sniff("x.jsonl", "{\"id\":\"a\",\"timestamp\":\"t\",\"instructions\":null}\n").unwrap().0, V_V0);
    assert_eq!(sniff("x.json", "{\n  \"session\": {},\n  \"items\": []\n}").unwrap().0, V_TS);
    assert!(sniff("x.jsonl", "{\"type\":\"user\",\"message\":{}}\n").is_none());
}

#[test]
fn agent_paths_are_valid() {
    let set = foreign_set();
    let child = set.get("sess-child").unwrap();
    assert_eq!(super::export::agent_path_for_test(child, &set), "/root/sess_child");
}

#[test]
#[ignore]
fn print_plain_neutral_loss() {
    let set = foreign_set();
    let (dir, out) = export_to_dir(&set, &ExportOptions::no_carry());
    let back = Codex.import(dir.path(), &ImportOptions { variant: None, restore_carried: false }).unwrap().set;
    let d = compare(&set, &back, CompareOptions { ignore_native: true, ..Default::default() });
    println!("{}\n{:#?}", d.summary(), out.losses);
}

/// A Codex thread continued in another harness and brought back: the native
/// lines are written verbatim, the new turn is rendered after them, and the
/// whole comes back event for event.
#[test]
fn continued_thread_appends_after_native_lines() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/codex/legacy-0.142-compaction-real/native");
    let mut set = Codex.import(&root, &ImportOptions::standard()).unwrap().set;
    let original = std::fs::read(rollout_files(&root).unwrap()[0].1.clone()).unwrap();
    let id = set.iter().next().unwrap().id().to_string();
    let mut l = set.ledgers.remove(&id).unwrap();
    for e in [
        ev("x1", Kind::Text, "2026-06-27T10:00:00.000Z").with("role", "user").with("text", "One more thing: run the tests."),
        ev("x2", Kind::ToolCall, "2026-06-27T10:00:01.000Z").with("call_id", "toolu_x").with("name", "Bash").with("input", json!({"command": "mvn test"})),
        ev("x3", Kind::ToolResult, "2026-06-27T10:00:05.000Z").with("call_id", "toolu_x").with("output", "BUILD SUCCESS"),
        ev("x4", Kind::Text, "2026-06-27T10:00:06.000Z").with("role", "assistant").with("text", "All green."),
    ] {
        l.append(e).unwrap();
    }
    set.insert(l);
    let (dir, out) = export_to_dir(&set, &ExportOptions::standard());
    let bytes = out.output.files.values().next().unwrap();
    assert!(bytes.starts_with(&original), "native lines are written back unchanged, first");
    let back = Codex.import(dir.path(), &ImportOptions::standard()).unwrap().set;
    assert_eq!(back.get(&id).unwrap().events, set.get(&id).unwrap().events);
    let again = Codex.export(&back, &ExportOptions::standard()).unwrap();
    assert_eq!(&again.output, &out.output);
    // The appended turn is a well-formed Codex turn.
    let tail: Vec<Value> = std::str::from_utf8(&bytes[original.len()..]).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let types: Vec<String> = tail.iter().map(|v| format!("{}/{}", v["type"].as_str().unwrap(), v["payload"]["type"].as_str().unwrap_or(""))).collect();
    assert_eq!(
        types,
        [
            "event_msg/task_started",
            "response_item/message",
            "event_msg/user_message",
            "response_item/function_call",
            "response_item/function_call_output",
            "response_item/message",
            "event_msg/agent_message",
            "event_msg/task_complete"
        ]
    );
}

fn one_ledger(id: &str, evs: Vec<Event>) -> LedgerSet {
    let mut l =
        Ledger::new(Agent { id: id.into(), created_at: Some("2026-01-01T00:00:00.000Z".into()), source: None, parent: None, links: vec![], meta: Obj::new() });
    for e in evs {
        l.append(e).unwrap();
    }
    LedgerSet::single(l)
}

fn lines_of(out: &crate::formats::Exported) -> Vec<Value> {
    let (_, bytes) = out.output.files.iter().find(|(k, _)| k.contains("rollout-")).unwrap();
    std::str::from_utf8(bytes).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

fn plain_trip(set: &LedgerSet) -> (crate::formats::Exported, LedgerSet) {
    let (dir, out) = export_to_dir(set, &ExportOptions::no_carry());
    let back = Codex.import(dir.path(), &ImportOptions { variant: None, restore_carried: false }).unwrap().set;
    (out, back)
}

fn kinds(l: &Ledger, k: Kind) -> Vec<&Event> {
    l.events.iter().filter(|e| e.kind == k).collect()
}

/// SPEC §3.1.1: neutral input excludes cache reads and writes; Codex's
/// `input_tokens` includes them. A response's usage becomes a
/// `token_usage_record` named by its group plus the `token_count` Codex writes
/// beside it, and comes back as one usage event.
#[test]
fn usage_follows_the_neutral_convention_and_is_counted_once() {
    let set = one_ledger(
        "u",
        vec![
            ev("p", Kind::Text, "2026-01-01T00:00:01Z").with("role", "user").with("text", "hi"),
            ev("a", Kind::Text, "2026-01-01T00:00:02Z").with("role", "assistant").with("text", "hello").group(Some("resp_9")),
            ev("us", Kind::Usage, "2026-01-01T00:00:02Z")
                .group(Some("resp_9"))
                .with("input", 10)
                .with("cache_read", 100)
                .with("cache_write", 0)
                .with("output", 20)
                .with("reasoning", 3),
        ],
    );
    let (out, back) = plain_trip(&set);
    let lines = lines_of(&out);
    let rec = lines.iter().find(|v| v["type"] == "token_usage_record").expect("a token_usage_record");
    assert_eq!(rec["payload"]["response_id"], "resp_9");
    assert_eq!(rec["payload"]["usage"]["input_tokens"], 110);
    assert_eq!(rec["payload"]["usage"]["cache_write_input_tokens"], 0);
    assert_eq!(rec["payload"]["usage"]["total_tokens"], 130);
    let tc = lines.iter().find(|v| v["payload"]["type"] == "token_count").unwrap();
    assert_eq!(tc["payload"]["info"]["last_token_usage"], rec["payload"]["usage"]);
    let l = back.iter().next().unwrap();
    let usage = kinds(l, Kind::Usage);
    assert_eq!(usage.len(), 1, "the token_count copy is not a second usage");
    let u = usage[0];
    for (k, v) in [("input", 10), ("cache_read", 100), ("output", 20), ("reasoning", 3)] {
        assert_eq!(u.body.get(k), Some(&json!(v)), "{k}");
    }
    // Codex writes every count, so its zero cache write reads as not stated.
    assert_eq!(u.body.get("cache_write"), None);
    // One response: the answer and its usage share a group. The record
    // naming it (`resp_9`) comes after the answer, so the group is the
    // answer's own (SPEC §9.3); `resp_9` stays in the usage's record.
    let answer = kinds(l, Kind::Text)[1];
    assert!(answer.group.is_some());
    assert_eq!(u.group, answer.group);
    assert_eq!(kinds(l, Kind::Text)[0].group, None, "the prompt is not part of the response");
    assert_eq!(u.native.as_ref().and_then(|n| n.record_json()).unwrap()["payload"]["response_id"], "resp_9");
}

/// A Codex rollout with both a `token_usage_record` and its `token_count`
/// (0.153 and later) gives one usage per response, in one group with the
/// response's output items that have no id of their own. The record comes
/// after them, so the group is the first item's (SPEC §9.3), not the record's
/// `response_id`, which stays in the usage's native record.
#[test]
fn codex_usage_record_and_token_count_are_one_response() {
    let u = r#"{"input_tokens":150,"cached_input_tokens":100,"cache_write_input_tokens":0,"output_tokens":7,"reasoning_output_tokens":2,"total_tokens":157}"#;
    let text = [
        r#"{"timestamp":"2026-01-01T00:00:00.000Z","type":"session_meta","payload":{"id":"0199a1b0-0000-7000-8000-000000000001","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/w","originator":"codex_cli_rs","cli_version":"0.153.4"}}"#.to_string(),
        r#"{"timestamp":"2026-01-01T00:00:01.000Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}"#.into(),
        r#"{"timestamp":"2026-01-01T00:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"go"}]}}"#.into(),
        r#"{"timestamp":"2026-01-01T00:00:02.000Z","type":"response_item","payload":{"type":"reasoning","summary":[],"encrypted_content":null}}"#.into(),
        r#"{"timestamp":"2026-01-01T00:00:02.000Z","type":"response_item","payload":{"type":"function_call","name":"exec","arguments":"{}","call_id":"c1"}}"#.into(),
        format!(r#"{{"timestamp":"2026-01-01T00:00:02.000Z","type":"token_usage_record","payload":{{"thread_id":"0199a1b0-0000-7000-8000-000000000001","turn_id":"t1","session_id":"0199a1b0-0000-7000-8000-000000000001","root_turn_id":"t1","response_id":"resp_1","usage":{u},"turn_token_usage":{u},"thread_token_usage":{u}}}}}"#),
        r#"{"timestamp":"2026-01-01T00:00:03.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"ok"}}"#.into(),
        format!(r#"{{"timestamp":"2026-01-01T00:00:03.000Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{u},"last_token_usage":{u},"model_context_window":null}},"rate_limits":null}}}}"#),
    ]
    .join("\n")
        + "\n";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    std::fs::write(&path, &text).unwrap();
    let set = Codex.import(&path, &ImportOptions::standard()).unwrap().set;
    let l = set.iter().next().unwrap();
    let usage = kinds(l, Kind::Usage);
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].body.get("input"), Some(&json!(50)), "input excludes the 100 cached");
    let reasoning = kinds(l, Kind::Reasoning)[0];
    assert_eq!(reasoning.group.as_deref(), Some(reasoning.id.as_str()), "the first output item names the response");
    assert_eq!(usage[0].group, reasoning.group);
    assert_eq!(kinds(l, Kind::ToolCall)[0].group, reasoning.group);
    assert_eq!(kinds(l, Kind::Text)[0].group, None, "the prompt is not part of the response");
    assert_eq!(usage[0].native.as_ref().and_then(|n| n.record_json()).unwrap()["payload"]["response_id"], "resp_1");
    assert_eq!(Codex.export(&set, &ExportOptions::standard()).unwrap().output.files.values().next().unwrap(), text.as_bytes());
}

/// SPEC §3.1.1: a running total is a `token_count` whose totals are the
/// cumulative usage and whose last-response counts are all zero; a refresh line
/// restating the totals keeps the context estimate for a resumed thread.
#[test]
fn cumulative_usage_is_a_running_total() {
    let set = one_ledger(
        "c",
        vec![
            ev("p", Kind::Text, "2026-01-01T00:00:01Z").with("role", "user").with("text", "hi"),
            ev("u1", Kind::Usage, "2026-01-01T00:00:02Z").with("input", 5).with("output", 2),
            ev("u2", Kind::Usage, "2026-01-01T00:00:03Z").with("input", 7).with("output", 9),
            ev("tot", Kind::Usage, "2026-01-01T00:00:04Z").with("input", 12).with("output", 11).with("cache_read", 40).with("cumulative", true),
        ],
    );
    let (out, back) = plain_trip(&set);
    let tcs: Vec<Value> = lines_of(&out).into_iter().filter(|v| v["payload"]["type"] == "token_count").collect();
    assert_eq!(tcs.len(), 4);
    assert_eq!(tcs[2]["payload"]["info"]["total_token_usage"]["input_tokens"], 52);
    assert_eq!(tcs[2]["payload"]["info"]["last_token_usage"]["total_tokens"], 0);
    assert_eq!(tcs[3]["payload"]["info"]["total_token_usage"], tcs[2]["payload"]["info"]["total_token_usage"]);
    assert_eq!(tcs[3]["payload"]["info"]["last_token_usage"]["total_tokens"], 16);
    let l = back.iter().next().unwrap();
    let usage = kinds(l, Kind::Usage);
    assert_eq!(usage.len(), 3);
    assert_eq!(usage[2].body.get("cumulative"), Some(&json!(true)));
    assert_eq!(usage[2].body.get("input"), Some(&json!(12)));
    assert_eq!(usage[2].body.get("cache_read"), Some(&json!(40)));
    assert_eq!(usage[1].body.get("cumulative"), Some(&json!(false)));
}

/// A group that is one output item and not shaped like an API id travels as
/// the item's id; an API-shaped one does not (it would be sent to OpenAI).
#[test]
fn single_item_group_is_the_item_id() {
    let set = one_ledger(
        "g",
        vec![
            ev("p", Kind::Text, "2026-01-01T00:00:01Z").with("role", "user").with("text", "hi"),
            ev("a", Kind::Text, "2026-01-01T00:00:02Z").with("role", "assistant").with("text", "one").group(Some("step-1")),
            ev("c", Kind::ToolCall, "2026-01-01T00:00:03Z").with("call_id", "k1").with("name", "Read").with("input", json!({})).group(Some("step-2")),
            ev("o", Kind::ToolResult, "2026-01-01T00:00:04Z").with("call_id", "k1").with("output", "x"),
            ev("b", Kind::Text, "2026-01-01T00:00:05Z").with("role", "assistant").with("text", "two").group(Some("toolu_9")),
        ],
    );
    let (out, back) = plain_trip(&set);
    let ids: Vec<Value> =
        lines_of(&out).into_iter().filter(|v| v["type"] == "response_item").map(|v| v["payload"].get("id").cloned().unwrap_or(Value::Null)).collect();
    assert_eq!(ids, [Value::Null, json!("step-1"), json!("step-2"), Value::Null, Value::Null]);
    let l = back.iter().next().unwrap();
    let evs: Vec<&Event> = l.events.iter().filter(|e| matches!(e.kind, Kind::Text | Kind::ToolCall)).collect();
    let groups: Vec<Option<&str>> = evs.iter().map(|e| e.group.as_deref()).collect();
    // The API-shaped one is not written; its item is grouped as a response of
    // its own, under its event id (SPEC §3.1.4, §9.3).
    assert_eq!(groups, [None, Some("step-1"), Some("step-2"), Some(evs[3].id.as_str())]);
}

/// A title is a `session_index.jsonl` line naming the thread; it comes back as
/// a title at its place in the thread, and a Codex index round-trips exactly.
#[test]
fn titles_are_session_index_lines() {
    let set = one_ledger(
        "t",
        vec![
            ev("p", Kind::Text, "2026-01-01T00:00:01Z").with("role", "user").with("text", "hi"),
            ev("n", Kind::Title, "2026-01-01T00:00:02Z").with("text", "Greeting"),
            ev("a", Kind::Text, "2026-01-01T00:00:03Z").with("role", "assistant").with("text", "hello"),
        ],
    );
    let (out, back) = plain_trip(&set);
    let index = std::str::from_utf8(&out.output.files[SESSION_INDEX]).unwrap().to_string();
    let v: Value = serde_json::from_str(index.lines().next().unwrap()).unwrap();
    assert_eq!(v["thread_name"], "Greeting");
    assert_eq!(v["updated_at"], "2026-01-01T00:00:02.000Z");
    let l = back.iter().next().unwrap();
    assert_eq!(v["id"].as_str(), l.agent().source.as_ref().unwrap().native_id.as_deref());
    let order: Vec<Kind> = l.events.iter().filter(|e| matches!(e.kind, Kind::Text | Kind::Title)).map(|e| e.kind).collect();
    assert_eq!(order, [Kind::Text, Kind::Title, Kind::Text]);
    // Codex's own index: read, then written back byte for byte.
    let dir = tempfile::tempdir().unwrap();
    out.output.write_to(dir.path(), true).unwrap();
    let native = Codex.import(dir.path(), &ImportOptions::standard()).unwrap();
    let again = Codex.export(&native.set, &ExportOptions::standard()).unwrap();
    assert_eq!(again.output, out.output);
    // With carry the title rides the rollout; the index line is not a second title.
    let (dir, _) = export_to_dir(&set, &ExportOptions::standard());
    let carried = Codex.import(dir.path(), &ImportOptions::standard()).unwrap().set;
    assert_eq!(carried.iter().next().unwrap().events, set.iter().next().unwrap().events);
}

/// SPEC §3.1.2: an MCP tool is a function call in its server's `mcp__<server>__`
/// namespace, as Codex calls one.
#[test]
fn mcp_calls_use_the_server_namespace() {
    let set = one_ledger(
        "m",
        vec![
            ev("p", Kind::Text, "2026-01-01T00:00:01Z").with("role", "user").with("text", "look"),
            ev("c", Kind::ToolCall, "2026-01-01T00:00:02Z")
                .with("call_id", "k1")
                .with("name", "screenshot")
                .with("tool_type", "mcp")
                .with("server", "browser")
                .with("input", json!({"full": true})),
            ev("o", Kind::ToolResult, "2026-01-01T00:00:03Z").with("call_id", "k1").with("output", "png"),
        ],
    );
    let (out, back) = plain_trip(&set);
    let call = lines_of(&out).into_iter().find(|v| v["payload"]["type"] == "function_call").unwrap();
    assert_eq!(call["payload"]["namespace"], "mcp__browser__");
    assert_eq!(call["payload"]["name"], "screenshot");
    let c = kinds(back.iter().next().unwrap(), Kind::ToolCall)[0].clone();
    assert_eq!(c.str("tool_type"), Some("mcp"));
    assert_eq!(c.str("server"), Some("browser"));
    assert_eq!(c.str("name"), Some("screenshot"));
    assert!(!c.body.contains_key("namespace"));
}

/// A file change Codex can state whole is the patch record Codex writes; one
/// it could state only in part is a named loss.
#[test]
fn file_changes_are_patch_records() {
    let evs = || {
        vec![
            ev("p", Kind::Text, "2026-01-01T00:00:01Z").with("role", "user").with("text", "write it"),
            ev("f1", Kind::FileChange, "2026-01-01T00:00:02Z").with("path", "/w/a.txt").with("op", "create").with("content", "hi\n"),
            ev("f2", Kind::FileChange, "2026-01-01T00:00:03Z").with("path", "/w/b.txt").with("op", "edit").with("diff", "@@ -1 +1 @@\n-a\n+b\n"),
            ev("f3", Kind::FileChange, "2026-01-01T00:00:04Z").with("path", "/w/c.txt").with("op", "snapshot"),
        ]
    };
    for (variant, want) in [(None, "event_msg/patch_apply_end"), (Some("paginated"), "event_msg/item_completed")] {
        let set = one_ledger("f", evs());
        let opts = ExportOptions { variant: variant.map(str::to_string), ..ExportOptions::no_carry() };
        let (dir, out) = export_to_dir(&set, &opts);
        let n = lines_of(&out)
            .iter()
            .filter(|v| format!("{}/{}", v["type"].as_str().unwrap(), v["payload"]["type"].as_str().unwrap_or("")) == want)
            .filter(|v| v["payload"]["type"] != "item_completed" || v["payload"]["item"]["type"] == "FileChange")
            .count();
        assert_eq!(n, 2, "{variant:?}");
        assert!(out.losses.0.iter().any(|l| l.code == "file-change-dropped" && l.count == 1));
        let back = Codex.import(dir.path(), &ImportOptions { variant: None, restore_carried: false }).unwrap().set;
        let fc: Vec<Obj> = kinds(back.iter().next().unwrap(), Kind::FileChange).iter().map(|e| e.body.clone()).collect();
        assert_eq!(fc, [evs()[1].body.clone(), evs()[2].body.clone()], "{variant:?}");
    }
}

/// A message the model never saw stays out of the model's history: a user or
/// assistant one is only the transcript's event; anything else is a named loss.
#[test]
fn record_only_messages_are_not_model_visible() {
    let mut note = ev("n", Kind::Text, "2026-01-01T00:00:00Z").with("role", "system").with("text", "Request cancelled.");
    note.audience = Some(crate::model::Audience::Record);
    let mut shown = ev("s", Kind::Text, "2026-01-01T00:00:02Z").with("role", "assistant").with("text", "No response requested.");
    shown.audience = Some(crate::model::Audience::Record);
    let set = one_ledger("r", vec![note, ev("p", Kind::Text, "2026-01-01T00:00:01Z").with("role", "user").with("text", "hi"), shown]);
    let (out, back) = plain_trip(&set);
    let lines = lines_of(&out);
    assert!(!lines.iter().any(|v| v["type"] == "response_item" && v["payload"]["role"] == "system"));
    assert!(!lines.iter().any(|v| v["type"] == "response_item" && v["payload"]["role"] == "assistant"));
    assert!(lines.iter().any(|v| v["payload"]["type"] == "agent_message" && v["payload"]["message"] == "No response requested."));
    assert!(out.losses.0.iter().any(|l| l.code == "record-message-dropped"));
    let texts: Vec<&str> = kinds(back.iter().next().unwrap(), Kind::Text).iter().filter_map(|e| e.str("text")).collect();
    assert_eq!(texts, ["hi", "No response requested."]);
}

/// The loss the first prefix-stable attempt (00a991f) caused. A message
/// people saw but the model did not (Claude Code's "No response requested.")
/// is exported as the transcript's copy alone, an `agent_message` (legacy) or
/// an `item_completed` `AgentMessage` (paginated), with no item after it. A UI
/// record read before its item cannot be told from it at its own line (SPEC
/// §9.3), so it waits; the next record that ends the response (here the next
/// prompt, or the turn's end) settles it, and its text comes back in the
/// conversation, once, in its place, as an assistant message.
#[test]
fn an_exported_record_only_agent_message_comes_back_in_the_conversation() {
    for variant in ["rollout-legacy", "rollout-paginated"] {
        for then_prompt in [false, true] {
            let mut shown = ev("s", Kind::Text, "2026-01-01T00:00:02Z").with("role", "assistant").with("text", "No response requested.");
            shown.audience = Some(crate::model::Audience::Record);
            let mut evs = vec![ev("p", Kind::Text, "2026-01-01T00:00:01Z").with("role", "user").with("text", "hi"), shown];
            if then_prompt {
                evs.push(ev("q", Kind::Text, "2026-01-01T00:00:03Z").with("role", "user").with("text", "next"));
            }
            let set = one_ledger("r", evs);
            let opts = ExportOptions { variant: Some(variant.into()), carry: false, target_version: None };
            let (dir, out) = export_to_dir(&set, &opts);
            let ui: Vec<Value> =
                lines_of(&out).into_iter().filter(|v| v["payload"]["type"] == "agent_message" || v["payload"]["item"]["type"] == "AgentMessage").collect();
            assert_eq!(ui.len(), 1, "{variant}: one transcript copy, no item: {ui:?}");
            let back = Codex.import(dir.path(), &ImportOptions { variant: None, restore_carried: false }).unwrap().set;
            let l = back.iter().next().unwrap();
            let texts: Vec<(&str, &str)> = kinds(l, Kind::Text).iter().map(|e| (e.str("role").unwrap_or(""), e.str("text").unwrap_or(""))).collect();
            let mut want = vec![("user", "hi"), ("assistant", "No response requested.")];
            if then_prompt {
                want.push(("user", "next"));
            }
            assert_eq!(texts, want, "{variant}, then_prompt={then_prompt}");
        }
    }
}

/// The placeholders Codex's required fields force on a thread that does not
/// state them (the epoch, `/`) read back as unknown; a summary's group is the
/// compaction's response id.
#[test]
fn placeholders_read_back_as_unknown_and_compaction_keeps_its_group() {
    let mut l = Ledger::new(Agent { id: "x".into(), created_at: None, source: None, parent: None, links: vec![], meta: Obj::new() });
    for e in [
        Event::new("p", Kind::Text).with("role", "user").with("text", "hi"),
        Event::new("c", Kind::Compaction).with("phase", "summary").with("summary", "we said hi").group(Some("resp_c1")),
    ] {
        l.append(e).unwrap();
    }
    let (out, back) = plain_trip(&LedgerSet::single(l));
    let meta = &lines_of(&out)[0]["payload"];
    assert_eq!(meta["cwd"], "/");
    assert_eq!(meta["timestamp"], super::UNKNOWN_TS);
    let b = back.iter().next().unwrap();
    assert_eq!(b.agent().created_at, None);
    assert!(b.events.iter().all(|e| e.ts.is_none()));
    assert!(!b.events.iter().any(|e| e.kind == Kind::Settings && e.body.contains_key("cwd")));
    assert_eq!(kinds(b, Kind::Compaction)[0].group.as_deref(), Some("resp_c1"));
}

/// SPEC §3.1.6: a peer outside the thread's agent tree (an orchestrated peer,
/// one on a network) is the wrapped user text the model was shown, and comes
/// back as the peer message; Codex's own channel is kept for agents of the tree.
#[test]
fn peers_outside_the_agent_tree_use_the_wrapper() {
    let set = one_ledger(
        "w",
        vec![
            ev("p", Kind::Text, "2026-01-01T00:00:01Z").with("role", "user").with("text", "start"),
            ev("m", Kind::PeerMessage, "2026-01-01T00:00:02Z")
                .with("direction", "in")
                .with("peer", "peer-a")
                .with("network", "audit-net")
                .with("text", "half done"),
            ev("o", Kind::PeerMessage, "2026-01-01T00:00:03Z").with("direction", "out").with("peer", "peer-a").with("text", "thanks"),
        ],
    );
    let (out, back) = plain_trip(&set);
    let lines = lines_of(&out);
    assert!(!lines.iter().any(|v| v["type"] == "inter_agent_communication"));
    let wrapped = "<peer-message direction=\"in\" peer=\"peer-a\" network=\"audit-net\">\nhalf done\n</peer-message>";
    assert!(lines.iter().any(|v| v["payload"]["role"] == "user" && v["payload"]["content"][0]["text"] == wrapped));
    assert!(out.losses.0.iter().any(|l| l.code == "peer-message-out-not-rendered"));
    let peers = kinds(back.iter().next().unwrap(), Kind::PeerMessage);
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].body, set.iter().next().unwrap().events[1].body);
    assert_eq!(super::peer_unwrap(wrapped), Some((Some("peer-a".into()), Some("audit-net".into()), "half done".into())));
    assert_eq!(super::peer_unwrap("<peer-message direction=\"in\" x=\"1\">\nhi\n</peer-message>"), None);
}

/// Rollout lines spelled the way a parsed `Value` does not re-spell them:
/// unsorted keys, `1e3` and `1.50`, the escape `\u00e9`, whitespace between tokens.
const ODD_ROLLOUT: [&str; 4] = [
    r#"{"timestamp":"2026-01-01T00:00:00.000Z","type":"session_meta","payload":{"id":"0199a1b0-0000-7000-8000-000000000001","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/w","originator":"codex_cli_rs","cli_version":"0.50.0"}}"#,
    r#"{"timestamp":"2026-01-01T00:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"caf\u00e9"}]}}"#,
    r#"{"timestamp":"2026-01-01T00:00:02.000Z","type": "response_item","payload":{"type":"function_call","name":"exec","arguments":"{\"cmd\":\"ls\",\"timeout\":1e3}","call_id":"c1","ratio":1.50}}"#,
    r#"{"timestamp":"2026-01-01T00:00:03.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"ol\u00e1"}}"#,
];

#[test]
fn self_trip_reemits_odd_spellings_exactly() {
    let text = ODD_ROLLOUT.join("\n") + "\n";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout-2026-01-01T00-00-00-0199a1b0-0000-7000-8000-000000000001.jsonl");
    std::fs::write(&path, &text).unwrap();
    // Each line is held as its own text.
    let set = Codex.import(&path, &ImportOptions::standard()).unwrap().set;
    let held: Vec<&str> =
        set.iter().next().unwrap().events.iter().filter_map(|e| e.native.as_ref()).filter(|n| n.part == 0).filter_map(|n| n.record_text()).collect();
    assert_eq!(held, ODD_ROLLOUT.to_vec());
    // X → P → X is byte-exact ...
    let st = crate::roundtrip::self_trip(&Codex, &path).unwrap();
    assert!(st.files.is_exact(), "self trip not exact: differ {:?} missing {:?} extra {:?}", st.files.differ, st.files.missing, st.files.extra);
    // ... and so is X → P → Y → P → X with carry, through the other primaries.
    for via in ["claude-code", "opencode"] {
        let y = crate::formats::by_name(via).unwrap();
        let ct = crate::roundtrip::cross_trip(&Codex, &path, y, true).unwrap();
        assert!(ct.back_files.is_exact(), "carry trip via {via} not exact: differ {:?}", ct.back_files.differ);
    }
}

/// Function-call arguments become a structured `input` exactly when they are
/// spelled as serde writes their value, whichever serde_json features are on.
#[test]
fn arguments_become_input_by_spelling_not_by_serde_features() {
    // Spelled as serde writes them, keys unsorted: structured.
    for args in [r#"{"cmd":"ls","a":1}"#, r#"{"z":{"y":[1,2.5,1e3]},"a":"caf\u00e9x"}"#.replace("\\u00e9", "\u{e9}").as_str(), "[]", "{}"] {
        assert!(super::import::compact_spelling(args), "{args}");
    }
    // Spelled otherwise: kept as text.
    for args in [r#"{"cmd": "ls"}"#, r#"{"a":"caf\u00e9"}"#, r#"{"a":1,"a":2}"#, "{\n}", r#"{"a":"\/"}"#] {
        assert!(!super::import::compact_spelling(args), "{args}");
    }
    let line = |args: &str| {
        format!(
            r#"{{"timestamp":"2026-01-01T00:00:02.000Z","type":"response_item","payload":{{"type":"function_call","name":"exec","arguments":{},"call_id":"c1"}}}}"#,
            serde_json::to_string(args).unwrap()
        )
    };
    let input_of = |args: &str| {
        let text = [ODD_ROLLOUT[0].to_string(), line(args)].join("\n") + "\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        std::fs::write(&path, &text).unwrap();
        let set = Codex.import(&path, &ImportOptions::standard()).unwrap().set;
        let call = set.iter().next().unwrap().events.iter().find(|e| e.kind == Kind::ToolCall).unwrap().clone();
        (call.body.get("input").cloned(), call.str("input_text").map(str::to_string))
    };
    assert_eq!(input_of(r#"{"cmd":"ls","a":1}"#), (Some(json!({"cmd": "ls", "a": 1})), None));
    assert_eq!(input_of(r#"{"cmd": "ls"}"#), (None, Some(r#"{"cmd": "ls"}"#.to_string())));
}

/// A TypeScript-CLI document (`JSON.stringify(doc, null, 2)`) whose records are
/// spelled as no parsed value re-spells them comes back byte for byte, from
/// the records' own texts: no whole-document copy, no `ts-json-raw-kept`.
#[test]
fn ts_json_document_reemits_odd_spellings_exactly() {
    let compact = r#"{"session":{"timestamp":"2025-04-17T14:50:24.646Z","id":"s-odd","instructions":""},"items":[{"role":"user","content":[{"type":"input_text","text":"caf\u00e9"}],"type":"message"},{"id":"rs_1","type":"reasoning","summary":[],"duration_ms":1e3,"ratio":1.50}]}"#;
    let text = crate::util::pretty(compact) + "\n";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout-2025-04-17-s-odd.json");
    std::fs::write(&path, &text).unwrap();
    let imp = Codex.import(&path, &ImportOptions::standard()).unwrap();
    assert!(imp.losses.0.is_empty(), "{:?}", imp.losses);
    let st = crate::roundtrip::self_trip(&Codex, &path).unwrap();
    assert!(st.files.is_exact(), "self trip not exact: differ {:?}", st.files.differ);
    for via in ["claude-code", "opencode"] {
        let ct = crate::roundtrip::cross_trip(&Codex, &path, crate::formats::by_name(via).unwrap(), true).unwrap();
        assert!(ct.back_files.is_exact(), "carry trip via {via} not exact: differ {:?}", ct.back_files.differ);
    }
}

// ---- branches (SPEC §2.3) ----

/// A rollout with a rollback, written from Codex's own record shapes
/// (openai/codex 0e67502): `RolloutLine {timestamp, type, payload}`, a
/// `response_item` user `message` per turn (the boundary
/// `user_message_positions_in_rollout` counts, `core/src/thread_rollout_truncation.rs:40-62`)
/// beside its `event_msg` `user_message`, and
/// `EventMsg::ThreadRolledBack {num_turns}` (`protocol/src/protocol.rs:1466-1515`,
/// `:3814-3817`), which `rollout/src/policy.rs:135` persists. No Codex-made
/// rollout with a rollback exists under `fixtures/codex/`.
fn rollback_rollout(num_turns: u32) -> String {
    let user = |t: &str, ts: &str| {
        [
            format!(
                r#"{{"timestamp":"{ts}","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{t}"}}]}}}}"#
            ),
            format!(r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"user_message","message":"{t}","images":[]}}}}"#),
        ]
    };
    let asst = |t: &str, ts: &str| {
        format!(
            r#"{{"timestamp":"{ts}","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"{t}"}}]}}}}"#
        )
    };
    let mut lines = vec![ODD_ROLLOUT[0].to_string()];
    lines.push(r#"{"timestamp":"2026-01-01T00:00:00.500Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>\n  <cwd>/w</cwd>\n</environment_context>"}]}}"#.into());
    lines.extend(user("write a haiku", "2026-01-01T00:00:01.000Z"));
    lines.push(asst("soft rain on the roof", "2026-01-01T00:00:02.000Z"));
    lines.extend(user("count its syllables", "2026-01-01T00:00:03.000Z"));
    lines.push(r#"{"timestamp":"2026-01-01T00:00:04.000Z","type":"response_item","payload":{"type":"function_call","name":"exec","arguments":"{\"cmd\":\"wc\"}","call_id":"c1"}}"#.into());
    lines.push(
        r#"{"timestamp":"2026-01-01T00:00:05.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"5"}}"#.into(),
    );
    lines.push(asst("five", "2026-01-01T00:00:06.000Z"));
    lines.push(format!(r#"{{"timestamp":"2026-01-01T00:00:07.000Z","type":"event_msg","payload":{{"type":"thread_rolled_back","num_turns":{num_turns}}}}}"#));
    lines.extend(user("translate it instead", "2026-01-01T00:00:08.000Z"));
    lines.push(asst("douce pluie", "2026-01-01T00:00:09.000Z"));
    lines.join("\n") + "\n"
}

fn import_rollout(text: &str) -> (tempfile::TempDir, std::path::PathBuf, Ledger) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout-2026-01-01T00-00-00-0199a1b0-0000-7000-8000-000000000001.jsonl");
    std::fs::write(&path, text).unwrap();
    let l = Codex.import(&path, &ImportOptions::standard()).unwrap().set.iter().next().unwrap().clone();
    (dir, path, l)
}

fn texts(evs: &[&Event]) -> Vec<String> {
    evs.iter().filter(|e| e.kind == Kind::Text).filter_map(|e| e.str("text")).map(str::to_string).collect()
}

/// (c) A `thread_rolled_back` record becomes an `after` branch back to just
/// before the last `num_turns` user turns: every rolled-back event stays in
/// the ledger, and the active path skips them. Codex's own `.jsonl` comes back
/// byte for byte, rollback and rolled-back turn included.
#[test]
fn a_rollback_is_an_after_branch_that_keeps_the_rolled_back_turns() {
    let text = rollback_rollout(1);
    let (_dir, path, l) = import_rollout(&text);
    assert_eq!(l.events.iter().filter(|e| e.native.as_ref().is_some_and(|n| n.part == 0)).count(), text.lines().count(), "every line is still an event");
    let rb = l.events.iter().position(|e| e.native.as_ref().and_then(|n| n.record_text()).is_some_and(|t| t.contains("thread_rolled_back"))).unwrap();
    let haiku = l.events.iter().position(|e| e.str("text") == Some("soft rain on the roof")).unwrap();
    assert_eq!(l.events[rb].after.as_deref(), Some(l.events[haiku].id.as_str()), "back to the end of the first turn");
    assert!(l.is_branched());
    let active = l.active_path();
    let all: Vec<&Event> = l.events.iter().collect();
    assert!(texts(&all).iter().any(|t| t == "five") && all.iter().any(|e| e.kind == Kind::ToolCall), "the rolled-back turn is kept");
    let distinct = |evs: &[&Event]| texts(evs).into_iter().collect::<std::collections::BTreeSet<_>>().into_iter().collect::<Vec<_>>();
    assert_eq!(
        distinct(&active),
        ["<environment_context>\n  <cwd>/w</cwd>\n</environment_context>", "douce pluie", "soft rain on the roof", "translate it instead", "write a haiku"]
    );
    assert!(!active.iter().any(|e| e.kind == Kind::ToolCall || e.str("text") == Some("five") || e.str("text") == Some("count its syllables")));
    assert!(active.iter().any(|e| e.str("text") == Some("douce pluie")) && active.iter().any(|e| e.id == l.events[rb].id));
    let st = crate::roundtrip::self_trip(&Codex, &path).unwrap();
    assert!(st.files.is_exact(), "self trip not exact: differ {:?}", st.files.differ);
    for via in ["claude-code", "opencode"] {
        let ct = crate::roundtrip::cross_trip(&Codex, &path, crate::formats::by_name(via).unwrap(), true).unwrap();
        assert_eq!(ct.there.score(), 0, "carried trip via {via} lost something");
        assert!(ct.back_files.is_exact(), "carry trip via {via} not exact: differ {:?}", ct.back_files.differ);
    }
}

/// Opposes the test above: `num_turns` counts user turns from the end, and
/// injected context is no turn; more turns than there are drops them all.
/// A rollout with no rollback has no branch.
#[test]
fn a_rollback_counts_user_turns_from_the_end() {
    for n in [2, 99] {
        let (_d, _p, l) = import_rollout(&rollback_rollout(n));
        let rb = l.events.iter().position(|e| e.native.as_ref().and_then(|x| x.record_text()).is_some_and(|t| t.contains("thread_rolled_back"))).unwrap();
        let env = l.events.iter().position(|e| e.str("text").is_some_and(|t| t.starts_with("<environment_context>"))).unwrap();
        assert_eq!(l.events[rb].after.as_deref(), Some(l.events[env].id.as_str()), "num_turns {n}");
        assert!(!l.active_path().iter().any(|e| e.str("text") == Some("soft rain on the roof")));
    }
    let (_d, _p, l) = import_rollout(&rollback_rollout(0));
    assert!(!l.is_branched(), "a rollback of no turns is no branch");
    let plain: String = rollback_rollout(1).lines().filter(|x| !x.contains("thread_rolled_back")).map(|x| format!("{x}\n")).collect();
    let (_d, _p, l) = import_rollout(&plain);
    assert!(!l.is_branched() && l.events.iter().all(|e| e.after.is_none()));
}

/// (b) A foreign branched ledger exports its active path only: no rolled-back
/// call, no abandoned text; with carry the abandoned events ride along and
/// come back with their branch.
#[test]
fn a_foreign_branch_exports_only_its_active_path() {
    let set = LedgerSet::read_path(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/protoledger/branch-rewind/native")).unwrap();
    for opts in [ExportOptions::no_carry(), ExportOptions::standard()] {
        let (dir, out) = export_to_dir(&set, &opts);
        let lines = lines_of(&out);
        let said: Vec<String> =
            lines.iter().filter(|v| v["type"] == "response_item").filter_map(|v| v["payload"]["content"][0]["text"].as_str().map(str::to_string)).collect();
        assert_eq!(
            said,
            [
                "Write a haiku about rain.",
                "Soft rain on the roof\ncounting every tired tile\nthe gutter hums low",
                "Never mind the count. Translate the haiku into French.",
                "Douce pluie sur le toit\ncomptant chaque tuile lasse\nla gouttière chante"
            ]
        );
        assert_eq!(out.losses.0.iter().find(|l| l.code == "abandoned-branch-not-rendered").map(|l| l.count), Some(4));
        assert!(!lines.iter().any(|v| v["payload"]["type"] == "function_call"), "the abandoned call was rendered");
        if opts.carry {
            let back = Codex.import(dir.path(), &ImportOptions::standard()).unwrap().set;
            assert_eq!(back.iter().next().unwrap().events, set.iter().next().unwrap().events);
        }
    }
}

/// A record held as its line's own text keeps no second copy in
/// `raw` — in a rollout and in a ts-json document, odd spellings included.
#[test]
fn a_record_held_as_its_text_keeps_no_raw() {
    let dir = tempfile::tempdir().unwrap();
    let rollout = dir.path().join("rollout-2026-01-01T00-00-00-0199a1b0-0000-7000-8000-000000000001.jsonl");
    std::fs::write(&rollout, ODD_ROLLOUT.join("\n") + "\n").unwrap();
    let doc = dir.path().join("rollout-2025-04-17-s-odd.json");
    let compact = r#"{"session":{"timestamp":"2025-04-17T14:50:24.646Z","id":"s-odd","instructions":""},"items":[{"role":"user","content":[{"type":"input_text","text":"café"}],"type":"message"},{"id":"rs_1","type":"reasoning","summary":[],"duration_ms":1e3,"ratio":1.50}]}"#;
    std::fs::write(&doc, crate::util::pretty(compact) + "\n").unwrap();
    // A session index (read by `read_session_index`, its own `raw` site).
    let indexed = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/codex/legacy-session-index-synthetic/native");
    for path in [rollout, doc, indexed.clone()] {
        let set = Codex.import(&path, &ImportOptions::standard()).unwrap().set;
        let natives: Vec<&crate::model::Native> = set.iter().flat_map(|l| &l.events).filter_map(|e| e.native.as_ref()).filter(|n| n.part == 0).collect();
        assert!(natives.len() >= 3);
        if path == indexed {
            assert_eq!(natives.iter().filter(|n| n.file.as_deref() == Some(SESSION_INDEX)).count(), 3, "the session index's lines are imported");
        }
        assert!(natives.iter().all(|n| n.record_text().is_some()), "{}", path.display());
        assert_eq!(natives.iter().filter(|n| n.raw.is_some()).count(), 0, "{}: a record held as its exact text also kept a raw copy", path.display());
    }
}

/// Opposes the test above: a line its record cannot hold as its text (a JSON
/// value with trailing whitespace is held trimmed) still keeps `raw`, and
/// comes back byte for byte.
#[test]
fn a_line_its_record_cannot_hold_keeps_raw() {
    let line = format!("{} ", ODD_ROLLOUT[1]);
    let text = [ODD_ROLLOUT[0].to_string(), line.clone()].join("\n") + "\n";
    let (_d, path, l) = import_rollout(&text);
    let n = l.events.iter().filter_map(|e| e.native.as_ref()).filter(|n| n.part == 0).nth(1).unwrap();
    assert_eq!((n.record_text(), n.raw.as_deref()), (Some(ODD_ROLLOUT[1]), Some(line.as_str())));
    let st = crate::roundtrip::self_trip(&Codex, &path).unwrap();
    assert!(st.files.is_exact(), "self trip not exact: differ {:?}", st.files.differ);
}

/// Opposes the session-index arm of the test above: a session index line its
/// record cannot hold as its text (trailing whitespace) still keeps `raw`,
/// and the index comes back byte for byte.
#[test]
fn a_session_index_line_its_record_cannot_hold_keeps_raw() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/codex/legacy-session-index-synthetic/native");
    let dir = tempfile::tempdir().unwrap();
    for e in walkdir::WalkDir::new(&src).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()) {
        let to = dir.path().join(e.path().strip_prefix(&src).unwrap());
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(e.path(), &to).unwrap();
    }
    let index = dir.path().join(SESSION_INDEX);
    let text = std::fs::read_to_string(&index).unwrap();
    let first = text.lines().next().unwrap().to_string();
    let spaced = format!("{first} ");
    std::fs::write(&index, text.replacen(&first, &spaced, 1)).unwrap();
    let set = Codex.import(dir.path(), &ImportOptions::standard()).unwrap().set;
    let n = set
        .iter()
        .flat_map(|l| &l.events)
        .filter_map(|e| e.native.as_ref())
        .find(|n| n.file.as_deref() == Some(SESSION_INDEX) && n.id.as_deref() == Some("@0"))
        .unwrap();
    assert_eq!((n.record_text(), n.raw.as_deref()), (Some(first.as_str()), Some(spaced.as_str())));
    let st = crate::roundtrip::self_trip(&Codex, dir.path()).unwrap();
    assert!(st.files.is_exact(), "self trip not exact: differ {:?}", st.files.differ);
}

/// The rollback rollout with `extra` lines spliced in just before the rollback.
fn rollback_with(num_turns: u32, extra: &[String]) -> String {
    let text = rollback_rollout(num_turns);
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let at = lines.iter().position(|l| l.contains("thread_rolled_back")).unwrap();
    for (k, x) in extra.iter().enumerate() {
        lines.insert(at + k, x.clone());
    }
    lines.join("\n") + "\n"
}

fn after_of_rollback(l: &Ledger) -> Option<String> {
    l.events.iter().find(|e| e.native.as_ref().and_then(|n| n.record_text()).is_some_and(|t| t.contains("thread_rolled_back"))).and_then(|e| e.after.clone())
}

fn id_of(l: &Ledger, pred: impl Fn(&str) -> bool) -> String {
    l.events
        .iter()
        .find(|e| e.native.as_ref().is_some_and(|n| n.part == 0) && e.native.as_ref().and_then(|n| n.record_text()).is_some_and(&pred))
        .unwrap()
        .id
        .clone()
}

/// Codex counts user turns the way it rebuilds history (openai/codex 0e67502,
/// `is_user_turn_boundary`, `core/src/context_manager/history.rs:1372-1387`;
/// `core/src/session/rollout_reconstruction.rs:436-440`, `:516-518`): an
/// `inter_agent_communication` record is a turn; a compaction summary written
/// as a user message is not; a compaction with a `replacement_history`
/// replaces the turns with the boundaries in it.
#[test]
fn a_rollback_counts_the_turns_codex_rebuilds() {
    // An inter-agent message after the second turn is a turn of its own: a
    // rollback of one drops only it.
    let iac = r#"{"timestamp":"2026-01-01T00:00:06.500Z","type":"inter_agent_communication","payload":{"author":"/root/worker","recipient":"/root","other_recipients":[],"content":"done","trigger_turn":true}}"#.to_string();
    let (_d, _p, l) = import_rollout(&rollback_with(1, &[iac]));
    let five = id_of(&l, |t| t.contains("\"five\""));
    assert_eq!(after_of_rollback(&l), Some(five), "inter_agent_communication is a turn");

    // A compaction summary persisted as a user message is no turn. The text is
    // Codex's own template (`prompts/templates/compact/summary_prefix.md` at
    // openai/codex 0e67502, which has no final newline), copied here
    // literally, then "\n" and the summary (`core/src/compact.rs:404`).
    let summary = user_line(&format!("{CODEX_SUMMARY_TEMPLATE}\nWe counted syllables."));
    let (_d, _p, l) = import_rollout(&rollback_with(1, &[summary]));
    let haiku_answer = id_of(&l, |t| t.contains("soft rain on the roof"));
    assert_eq!(after_of_rollback(&l), Some(haiku_answer), "a compaction summary is no turn");

    // A compaction that keeps only the first prompt: the turns are then that
    // prompt and what follows, so a rollback of two drops back past the first.
    let first_prompt = r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"write a haiku"}]}"#;
    let compacted =
        format!(r#"{{"timestamp":"2026-01-01T00:00:06.500Z","type":"compacted","payload":{{"message":"counted","replacement_history":[{first_prompt}]}}}}"#);
    let mut lines: Vec<String> = rollback_rollout(2).lines().map(str::to_string).collect();
    let rb = lines.iter().position(|l| l.contains("thread_rolled_back")).unwrap();
    let after_rb = lines.split_off(rb + 1);
    let rb_line = lines.pop().unwrap();
    lines.push(compacted);
    lines.extend(after_rb.iter().take(2).cloned()); // the next prompt (response_item + event_msg)
    lines.push(rb_line);
    lines.extend(after_rb.into_iter().skip(2));
    let (_d, _p, l) = import_rollout(&(lines.join("\n") + "\n"));
    let env = id_of(&l, |t| t.contains("<environment_context>"));
    assert_eq!(after_of_rollback(&l), Some(env), "the compaction's replacement history is the turns");
}

/// `prompts/templates/compact/summary_prefix.md` at openai/codex 0e67502,
/// byte for byte (the file has no final newline).
const CODEX_SUMMARY_TEMPLATE: &str = "Another language model started to solve this problem and produced a summary of its thinking process. You also have access to the state of the tools that were used by that language model. Use this to build on the work that has already been done and avoid duplicating work. Here is the summary produced by the other language model, use the information in this summary to assist with your own analysis:";

/// A `response_item` user message with one `input_text` part.
fn user_line(text: &str) -> String {
    let part = json!({"type": "input_text", "text": text});
    json!({"timestamp": "2026-01-01T00:00:06.500Z", "type": "response_item", "payload": {"type": "message", "role": "user", "content": [part]}}).to_string()
}

/// A `response_item` assistant message whose one part has type `ty` and text `text`.
fn assistant_line(ty: &str, text: &str) -> String {
    let part = json!({"type": ty, "text": text});
    json!({"timestamp": "2026-01-01T00:00:06.500Z", "type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [part]}})
        .to_string()
}

/// What a rollback of one makes `after` when `extra` is spliced in just
/// before it: `Turn` when `extra` is a turn of its own (the rollback drops
/// only it, back to "five"), `NotTurn` when it is not (the rollback drops the
/// "count its syllables" turn, back to the haiku's answer).
#[derive(Debug, PartialEq)]
enum Counted {
    Turn,
    NotTurn,
    Other(Option<String>),
}

fn counted(extra: &str) -> Counted {
    let (_d, _p, l) = import_rollout(&rollback_with(1, &[extra.to_string()]));
    let five = id_of(&l, |t| t.contains("\"five\""));
    let haiku_answer = id_of(&l, |t| t.contains("soft rain on the roof"));
    match after_of_rollback(&l) {
        Some(a) if a == five => Counted::Turn,
        Some(a) if a == haiku_answer => Counted::NotTurn,
        a => Counted::Other(a),
    }
}

/// Codex's `is_summary_message` is the WHOLE template plus "\n": a user
/// message that starts with only the template's first sentence is no summary,
/// so it is a turn (the rule this importer had before matched it as one).
#[test]
fn only_codex_s_whole_summary_template_is_a_summary() {
    assert_eq!(counted(&user_line(&format!("{CODEX_SUMMARY_TEMPLATE}\nWe counted syllables."))), Counted::NotTurn);
    let first_sentence = CODEX_SUMMARY_TEMPLATE.split_inclusive(". ").next().unwrap().trim_end();
    assert_eq!(first_sentence, "Another language model started to solve this problem and produced a summary of its thinking process.");
    assert_eq!(counted(&user_line(&format!("{first_sentence}\nWe counted syllables."))), Counted::Turn, "the first sentence alone is no summary");
    // The template with no newline after it is no summary either (`starts_with("{SUMMARY_PREFIX}\n")`).
    assert_eq!(counted(&user_line(&format!("{CODEX_SUMMARY_TEMPLATE} We counted syllables."))), Counted::Turn);
    // A plain user prompt is a turn, as the rollout's own prompts are.
    assert_eq!(counted(&user_line("and a limerick")), Counted::Turn);
}

/// `ResponseItem::AgentMessage` is always a turn (`history.rs:1373-1375`).
#[test]
fn an_agent_message_item_is_a_turn() {
    let am = json!({"timestamp": "2026-01-01T00:00:06.500Z", "type": "response_item", "payload": {"type": "agent_message", "author": "/root/worker", "recipient": "/root", "content": [{"type": "input_text", "text": "done"}]}}).to_string();
    assert_eq!(counted(&am), Counted::Turn);
    // Opposed: the same item as a reasoning item is no turn.
    let not = am.replace("\"agent_message\"", "\"reasoning\"");
    assert_eq!(counted(&not), Counted::NotTurn);
}

/// An assistant message is a turn exactly when its content is one
/// `input_text`/`output_text` part holding an `InterAgentCommunication`
/// (`protocol.rs:1027-1038`): `author`/`recipient` agent paths, string
/// `content`, and `trigger_turn`, which has no serde default.
#[test]
fn an_assistant_message_is_a_turn_only_when_codex_reads_an_inter_agent_instruction() {
    let iac = r#"{"author":"/root/worker","recipient":"/root","other_recipients":[],"content":"done","trigger_turn":false}"#;
    assert_eq!(counted(&assistant_line("output_text", iac)), Counted::Turn);
    assert_eq!(counted(&assistant_line("input_text", iac)), Counted::Turn);
    // The importer's earlier, wider rule counted each of these; Codex does not.
    let no_trigger = r#"{"author":"/root/worker","recipient":"/root","content":"done"}"#;
    assert_eq!(counted(&assistant_line("output_text", no_trigger)), Counted::NotTurn, "no trigger_turn: not an InterAgentCommunication");
    assert_eq!(counted(&assistant_line("text", iac)), Counted::NotTurn, "a part type Codex has no ContentItem for");
    let bad_path = r#"{"author":"worker","recipient":"/root","content":"done","trigger_turn":true}"#;
    assert_eq!(counted(&assistant_line("output_text", bad_path)), Counted::NotTurn, "author is not an agent path");
    let object_content = r#"{"author":"/root/worker","recipient":"/root","content":{"t":"done"},"trigger_turn":true}"#;
    assert_eq!(counted(&assistant_line("output_text", object_content)), Counted::NotTurn, "content is not a string");
    // Plain assistant text is no turn.
    assert_eq!(counted(&assistant_line("output_text", "just words")), Counted::NotTurn);
}

/// A legacy compaction (no `replacement_history`) rebuilds history from the
/// user messages before it, so the turns before it stand
/// (`rollout_reconstruction.rs:520-545`): a rollback of one after it still
/// drops the last prompt before it.
#[test]
fn a_legacy_compaction_leaves_the_turns_standing() {
    let legacy = json!({"timestamp": "2026-01-01T00:00:06.500Z", "type": "compacted", "payload": {"message": "counted"}}).to_string();
    assert_eq!(counted(&legacy), Counted::NotTurn);
    // Opposed: a compaction that keeps nothing replaces the turns with none,
    // so the rollback has no turn to drop.
    let empty = json!({"timestamp": "2026-01-01T00:00:06.500Z", "type": "compacted", "payload": {"message": "counted", "replacement_history": []}}).to_string();
    assert_eq!(counted(&empty), Counted::Other(None));
}

/// A `protoledger` member that is not shaped like
/// a carrier is the line's own data. It survives the import (in the record,
/// byte for byte) and a Codex self trip writes the file back exactly. Opposed:
/// a carrier-shaped member whose hash no longer matches is still stripped,
/// and the line is imported fresh.
#[test]
fn a_genuine_protoledger_member_survives_a_codex_trip() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/codex/v0-0.20-synthetic/native/rollout-2025-08-08T17-24-21-5973b6c0-94b8-487b-a530-2aeb6098ae0e.jsonl");
    let text = std::fs::read_to_string(&src).unwrap();
    let line = text.lines().find(|l| l.contains("\"type\":\"message\"") && l.contains("\"role\":\"user\"")).unwrap().to_string();
    for (member, genuine) in [(r#""protoledger":"genuine","#, true), (r#""protoledger":{"v":"0.1","h":"stale","events":[]},"#, false)] {
        let planted = line.replacen('{', &format!("{{{member}"), 1);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(src.file_name().unwrap());
        std::fs::write(&path, text.replacen(&line, &planted, 1)).unwrap();
        let imp = Codex.import(&path, &ImportOptions::standard()).unwrap();
        let held = imp.set.iter().flat_map(|l| l.events.iter()).filter_map(|e| e.native.as_ref()).any(|n| n.record_text() == Some(planted.as_str()));
        assert_eq!(held, genuine, "member {member}: the line's record kept its own text");
        let ex = Codex.export(&imp.set, &ExportOptions::standard()).unwrap();
        let back = String::from_utf8(ex.output.files.values().next().unwrap().clone()).unwrap();
        assert_eq!(back.contains(&planted), genuine, "member {member}: the self trip wrote the line back as it was");
    }
}
