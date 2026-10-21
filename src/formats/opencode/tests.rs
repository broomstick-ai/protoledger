//! Unit tests for the opencode adapter.

use super::*;
use crate::formats::{ExportOptions, Format, ImportOptions};
use crate::loss::{CompareOptions, compare};
use crate::model::{Agent, Event, Kind, Parent, Relation};
use crate::roundtrip::materialise;
use serde_json::json;

fn agent(id: &str) -> Agent {
    Agent { id: id.into(), created_at: Some("2026-05-01T10:00:00.000Z".into()), source: None, parent: None, links: vec![], meta: Obj::new() }
}

fn ev(id: &str, kind: Kind, ts_: &str, group: &str) -> Event {
    Event::new(id, kind).ts(Some(ts_)).group(Some(group))
}

/// A ledger set shaped like one imported from another harness: a parent with
/// text, reasoning, a tool round trip, usage and a title, and a subagent child.
fn foreign_set() -> LedgerSet {
    let mut p = Ledger::new(agent("parent-agent"));
    p.agent_mut().meta.insert("app".into(), json!({"k": 1.5}));
    p.agent_mut().links.push(Link { rel: "spawned-by".into(), agent: Some("orchestrator".into()), event: None, extra: Obj::new() });
    let evs = vec![
        ev("e1", Kind::Settings, "2026-05-01T10:00:00.000Z", "s")
            .with("model", "claude-sonnet-4-5")
            .with("cwd", "/home/user/project-a")
            .with("permission_mode", "default"),
        ev("e2", Kind::Text, "2026-05-01T10:00:01.000Z", "u1").with("role", "user").with("text", "Read input.txt"),
        ev("e3", Kind::Reasoning, "2026-05-01T10:00:02.000Z", "a1")
            .with("text", "I will read it.")
            .with("sealed", json!({"vendor": "anthropic", "kind": "signature", "data": "sig"})),
        ev("e4", Kind::ToolCall, "2026-05-01T10:00:02.500Z", "a1")
            .with("call_id", "toolu_1")
            .with("name", "Read")
            .with("input", json!({"file_path": "input.txt"})),
        ev("e5", Kind::ToolResult, "2026-05-01T10:00:03.000Z", "u2").with("call_id", "toolu_1").with("output", "alpha\nbeta\ngamma\n").with("is_error", false),
        ev("e6", Kind::ToolCall, "2026-05-01T10:00:04.000Z", "a2")
            .with("call_id", "toolu_2")
            .with("name", "Task")
            .with("input", json!({"description": "summarise", "prompt": "Summarise notes"})),
        ev("e7", Kind::ToolResult, "2026-05-01T10:00:09.000Z", "u3")
            .with("call_id", "toolu_2")
            .with("output", json!([{"type": "text", "text": "A plan."}]))
            .with("is_error", false),
        ev("e8", Kind::Text, "2026-05-01T10:00:10.000Z", "a3").with("role", "assistant").with("text", "Three words: alpha beta gamma."),
        ev("e9", Kind::Usage, "2026-05-01T10:00:10.000Z", "a3").with("input", 120).with("output", 30.0).with("cache_read", 5).with("cost_usd", 0.0012),
        ev("e10", Kind::Title, "2026-05-01T10:00:11.000Z", "t").with("text", "Read a file"),
        ev("e11", Kind::Turn, "2026-05-01T10:00:11.000Z", "t").with("phase", "end").with("outcome", "ok"),
        ev("e12", Kind::FileChange, "2026-05-01T10:00:12.000Z", "f").with("path", "out.txt").with("op", "create"),
        ev("e13", Kind::Text, "2026-05-01T10:00:13.000Z", "u4").with("role", "user").with("text", "Thanks").meta_ns("app", json!({"raw": "Thanks"})),
    ];
    for e in evs {
        p.append(e).unwrap();
    }
    let mut c = Ledger::new(Agent {
        parent: Some(Parent { agent: "parent-agent".into(), event: Some("e6".into()), relation: Relation::Subagent }),
        created_at: Some("2026-05-01T10:00:05.000Z".into()),
        ..agent("child-agent")
    });
    c.append(ev("c1", Kind::Text, "2026-05-01T10:00:05.000Z", "cu").with("role", "user").with("text", "Summarise notes")).unwrap();
    c.append(ev("c2", Kind::Text, "2026-05-01T10:00:08.000Z", "ca").with("role", "assistant").with("text", "A plan.")).unwrap();
    let mut set = LedgerSet::new();
    set.insert(p);
    set.insert(c);
    set
}

/// `set` without its opencode store ledgers and the `member-of` links that
/// name a store ledger: the conversation an `opencode.db` holds. A database written
/// from rendered sessions holds the project rows the writer made for them, so
/// a round trip through one gains a store ledger; only a database keeps one.
#[cfg(feature = "sqlite")] // used only by the SQLite tests
fn without_store(set: &LedgerSet) -> LedgerSet {
    // A header carried through another container still names its store.
    let store_id = |a: &str| a == store_ledger_id(None) || a.starts_with(&format!("{}:", store_ledger_id(None)));
    let mut out = LedgerSet::new();
    for l in set.iter().filter(|l| !is_store_ledger(l)) {
        let mut l = l.clone();
        l.agent_mut().links.retain(|k| !(k.rel == "member-of" && k.agent.as_deref().is_some_and(store_id)));
        out.insert(l);
    }
    out
}

#[cfg(feature = "sqlite")] // used only by the SQLite tests
const ALL: [&str; 5] = ["v2-transfer", "v2-sqlite", "v1-export", "v1-storage-json", "v1-sqlite"];

fn export_import(set: &LedgerSet, variant: &str, carry: bool) -> (LedgerSet, crate::formats::Output) {
    let (a, b, _) = export_import_losses(set, variant, carry);
    (a, b)
}

fn export_import_losses(set: &LedgerSet, variant: &str, carry: bool) -> (LedgerSet, crate::formats::Output, crate::formats::Losses) {
    let opts = ExportOptions { variant: Some(variant.into()), carry, target_version: None };
    let out = Opencode.export(set, &opts).unwrap();
    let losses = out.losses.clone();
    let dir = tempfile::tempdir().unwrap();
    let path = materialise(&out.output, dir.path()).unwrap();
    let detected = Opencode.detect(&path).unwrap();
    assert_eq!(detected.variant, variant, "detects its own export");
    let back = Opencode.import(&path, &ImportOptions { variant: None, restore_carried: carry }).unwrap();
    (back.set, out.output, losses)
}

#[cfg(feature = "sqlite")] // reads or writes a SQLite store
#[test]
fn foreign_ledgers_come_back_exact_with_carry() {
    let set = foreign_set();
    for v in ALL {
        let (back, _) = export_import(&set, v, true);
        let back = without_store(&back);
        let d = compare(&set, &back, CompareOptions { ignore_native: true, ..Default::default() });
        assert!(d.is_clean(), "{v}: {}", d.summary());
        assert_eq!(back.get("child-agent").unwrap().agent().parent.as_ref().unwrap().agent, "parent-agent", "{v}");
    }
}

#[cfg(feature = "sqlite")] // reads or writes a SQLite store
#[test]
fn foreign_ledgers_without_carry_keep_the_conversation() {
    let set = foreign_set();
    for v in ALL {
        let (back, out) = export_import(&set, v, false);
        let back = without_store(&back);
        assert_eq!(back.len(), 2, "{v}");
        let parent = back.iter().find(|l| l.agent().parent.is_none()).unwrap();
        let child = back.iter().find(|l| l.agent().parent.is_some()).unwrap();
        // The child is linked to the tool call that spawned it.
        let pe = child.agent().parent.as_ref().unwrap();
        assert_eq!(pe.agent, parent.id(), "{v}");
        let call = parent.events.iter().find(|e| e.kind == Kind::ToolCall && e.str("call_id") == Some("toolu_2")).unwrap();
        assert_eq!(pe.event.as_deref(), Some(call.id.as_str()), "{v}");
        let texts: Vec<&str> = parent.events.iter().filter(|e| e.kind == Kind::Text).filter_map(|e| e.str("text")).collect();
        assert!(texts.contains(&"Read input.txt") && texts.contains(&"Three words: alpha beta gamma.") && texts.contains(&"Thanks"), "{v}: {texts:?}");
        let results: Vec<&Event> = parent.events.iter().filter(|e| e.kind == Kind::ToolResult).collect();
        assert_eq!(results.len(), 2, "{v}");
        assert!(parent.events.iter().any(|e| e.kind == Kind::Title && e.str("text") == Some("Read a file")), "{v}");
        if v.starts_with("v2") {
            for (name, bytes) in &out.files {
                if name.ends_with(".json") {
                    let doc: Value = serde_json::from_slice(bytes).unwrap();
                    validate_transfer(&doc).unwrap_or_else(|e| panic!("{v} {name}: {e}"));
                }
            }
        }
    }
}

/// The rules opencode 2.x enforces on `POST /api/experimental/session/import`
/// (`SessionTransfer.Data`, `packages/schema/src/session-message.ts`) that a
/// renderer can get wrong.
fn validate_transfer(doc: &Value) -> Result<(), String> {
    let info = doc.get("info").ok_or("no info")?;
    let sid = s(info, "id").ok_or("no session id")?;
    if !ids::is_canonical(sid, "ses") {
        return Err(format!("session id {sid}"));
    }
    for k in ["projectID", "cost", "tokens", "time", "location"] {
        info.get(k).ok_or(format!("info.{k} missing"))?;
    }
    if !s(&info["location"], "directory").is_some_and(render::is_absolute) {
        return Err("location.directory is not absolute".into());
    }
    let mut last = String::new();
    for m in doc["messages"].as_array().ok_or("no messages")? {
        let id = s(m, "id").ok_or("message id")?;
        if !ids::is_canonical(id, "msg") || id <= last.as_str() {
            return Err(format!("message id {id} after {last}"));
        }
        last = id.to_string();
        m["time"]["created"].as_i64().ok_or("time.created")?;
        match s(m, "type").ok_or("type")? {
            "assistant" => {
                m["time"]["completed"].as_i64().ok_or("unsettled assistant")?;
                s(m, "agent").ok_or("agent")?;
                s(&m["model"], "id").ok_or("model.id")?;
                s(&m["model"], "providerID").ok_or("model.providerID")?;
                for c in m["content"].as_array().ok_or("content")? {
                    if s(c, "type") == Some("tool") {
                        let st = &c["state"];
                        match s(st, "status") {
                            Some("completed") => {
                                if st["content"].as_array().is_none_or(|a| a.is_empty()) {
                                    return Err("completed tool needs non-empty content".into());
                                }
                            }
                            Some("error") => {
                                s(&st["error"], "type").ok_or("error.type")?;
                                s(&st["error"], "message").ok_or("error.message")?;
                            }
                            other => return Err(format!("tool state {other:?}")),
                        }
                        st["input"].as_object().ok_or("tool input must be an object")?;
                        c["time"]["created"].as_i64().ok_or("tool time.created")?;
                    }
                }
            }
            "user" => {
                s(m, "text").ok_or("user text")?;
                for f in m.get("files").and_then(Value::as_array).into_iter().flatten() {
                    s(f, "data").ok_or("file data")?;
                    s(f, "mime").ok_or("file mime")?;
                }
            }
            "compaction" => {
                if s(m, "status") == Some("running") {
                    return Err("running compaction".into());
                }
            }
            "synthetic" | "system" | "idle" | "model-switched" | "agent-switched" | "location-switched" | "skill" | "shell" => {}
            other => return Err(format!("unknown type {other}")),
        }
    }
    Ok(())
}

#[cfg(feature = "sqlite")] // reads or writes a SQLite store
#[test]
fn sqlite_v2_export_has_contiguous_seq_and_event_sequence() {
    let set = foreign_set();
    let out = Opencode.export(&set, &ExportOptions { variant: Some("v2-sqlite".into()), carry: false, target_version: None }).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = materialise(&out.output, dir.path()).unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    let mut st = db.prepare("SELECT session_id, group_concat(seq) FROM (SELECT * FROM session_message ORDER BY seq) GROUP BY session_id").unwrap();
    let rows: Vec<(String, String)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(rows.len(), 2);
    for (sid, seqs) in rows {
        let v: Vec<i64> = seqs.split(',').map(|x| x.parse().unwrap()).collect();
        assert_eq!(v, (1..=v.len() as i64).collect::<Vec<_>>(), "{sid}");
        let last: i64 = db.query_row("SELECT seq FROM event_sequence WHERE aggregate_id = ?1", [&sid], |r| r.get(0)).unwrap();
        assert_eq!(last, v.len() as i64);
    }
    // Every migration is recorded, so opencode 2.x opens it without migrating.
    let n: i64 = db.query_row("SELECT COUNT(*) FROM migration", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 50);
    let parent: Option<String> = db.query_row("SELECT parent_id FROM session_v2 WHERE parent_id IS NOT NULL", [], |r| r.get(0)).unwrap();
    assert!(parent.is_some());
}

#[cfg(feature = "sqlite")] // reads or writes a SQLite store
#[test]
fn reimport_is_stable() {
    let set = foreign_set();
    for v in ALL {
        let (a, out1) = export_import(&set, v, false);
        let out2 = Opencode.export(&a, &ExportOptions { variant: Some(v.into()), carry: false, target_version: None }).unwrap();
        if !v.ends_with("sqlite") {
            assert_eq!(out1, out2.output, "{v}: exporting an import of an export reproduces it");
        }
        let dir = tempfile::tempdir().unwrap();
        let p = materialise(&out2.output, dir.path()).unwrap();
        let b = Opencode.import(&p, &ImportOptions::standard()).unwrap().set;
        let d = compare(&a, &b, CompareOptions::default());
        assert!(d.is_clean(), "{v}: {}", d.summary());
    }
}

#[test]
fn edited_message_is_imported_fresh() {
    let set = foreign_set();
    let out = Opencode.export(&set, &ExportOptions { variant: Some("v2-transfer".into()), carry: true, target_version: None }).unwrap();
    let (name, bytes) = out.output.files.iter().find(|(_, b)| String::from_utf8_lossy(b).contains("Read input.txt")).unwrap();
    let mut doc: Value = serde_json::from_slice(bytes).unwrap();
    let msgs = doc["messages"].as_array_mut().unwrap();
    let u = msgs.iter_mut().find(|m| s(m, "text") == Some("Read input.txt")).unwrap();
    u["text"] = json!("Read input.txt please");
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join(name);
    std::fs::write(&p, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    let back = Opencode.import(&p, &ImportOptions::standard()).unwrap().set;
    let l = back.get("parent-agent").unwrap();
    assert!(l.events.iter().any(|e| e.str("text") == Some("Read input.txt please")));
    assert!(!l.events.iter().any(|e| e.str("text") == Some("Read input.txt")));
    // Untouched messages still restore their original events.
    assert!(l.contains("e8"));
}

#[test]
fn copied_carrier_is_not_restored() {
    // opencode copies session metadata into children and forks; the carrier is
    // only honoured on the session it was written for.
    let info = json!({"id": "ses_b", "metadata": {"protoledger": {"session": "ses_a", "agent": {"id": "x"}}}});
    assert!(header_carrier(&info, Gen::V2).is_none());
    let info = json!({"id": "ses_a", "metadata": {"protoledger": {"session": "ses_a", "agent": {"id": "x"}}}});
    assert!(header_carrier(&info, Gen::V2).is_some());
}

#[test]
fn js_numbers() {
    let mut v = json!({"a": 30.0, "b": 0.5, "c": [1.0, 2]});
    js_normalize(&mut v);
    assert_eq!(serde_json::to_string(&v).unwrap(), r#"{"a":30,"b":0.5,"c":[1,2]}"#);
}

/// The differences between an opencode.db fixture and its self trip: the
/// import's losses, then every table (schema, and rows as a sorted dump)
/// that does not come back identical. Empty means the store is row-identical.
#[cfg(feature = "sqlite")] // reads or writes a SQLite store
fn sqlite_self_trip_differences(db: &std::path::Path) -> Vec<String> {
    let imported = Opencode.import(db, &ImportOptions::standard()).unwrap();
    let mut diffs: Vec<String> = imported.losses.0.iter().map(|l| format!("import loss {} ×{}", l.code, l.count)).collect();
    diffs.extend(export_differences(&imported.set, db));
    diffs
}

/// Export `set` to an opencode.db (its own variant) and name every table of
/// `db` whose schema or rows the export does not reproduce.
#[cfg(feature = "sqlite")] // reads or writes a SQLite store
fn export_differences(set: &LedgerSet, db: &std::path::Path) -> Vec<String> {
    let out = match Opencode.export(set, &ExportOptions::standard()) {
        Ok(out) => out,
        Err(e) => return vec![format!("export failed: {e:#}")],
    };
    let dir = tempfile::tempdir().unwrap();
    let p = materialise(&out.output, dir.path()).unwrap();
    let p = if p.is_dir() { p.join("opencode.db") } else { p };
    table_differences(db, &p)
}

/// Every table of `a` whose schema or rows (as a sorted dump) differ in `b`.
#[cfg(feature = "sqlite")] // reads or writes a SQLite store
fn table_differences(a: &std::path::Path, b: &std::path::Path) -> Vec<String> {
    let a = rusqlite::Connection::open(a).unwrap();
    let b = rusqlite::Connection::open(b).unwrap();
    let mut diffs = Vec::new();
    let tables: Vec<String> = {
        let mut st = a.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").unwrap();
        st.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
    };
    let schema = |c: &rusqlite::Connection| -> Vec<String> {
        let mut st = c.prepare("SELECT sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY name").unwrap();
        st.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
    };
    if schema(&a) != schema(&b) {
        diffs.push("schema".into());
    }
    // Every table comes back row for row: the session model's, the migration
    // journals, and every other table (event log, worktrees, instructions, ...).
    for t in tables {
        let (want, got) = (dump_table(&a, &t), dump_table(&b, &t));
        if want != got {
            diffs.push(format!("table {t}: {} rows, {} back", want.len(), got.len()));
        }
    }
    diffs
}

/// The rows of `t`, one string each, sorted.
#[cfg(feature = "sqlite")] // reads or writes a SQLite store
fn dump_table(c: &rusqlite::Connection, t: &str) -> Vec<String> {
    let Ok(mut st) = c.prepare(&format!("SELECT * FROM \"{t}\"")) else { return vec!["(no such table)".into()] };
    let n = st.column_count();
    let mut rows: Vec<String> = st
        .query_map([], |r| {
            let mut s = String::new();
            for i in 0..n {
                match r.get_ref(i).unwrap() {
                    rusqlite::types::ValueRef::Text(t) => s.push_str(&format!("T:{}|", String::from_utf8_lossy(t))),
                    other => s.push_str(&format!("{other:?}|")),
                }
            }
            Ok(s)
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    rows.sort();
    rows
}

#[cfg(feature = "sqlite")] // used only by the SQLite tests
fn fixture_db(case: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/opencode").join(case).join("native/opencode.db")
}

/// Every opencode.db fixture re-exports to a db whose rows are identical.
#[cfg(feature = "sqlite")] // reads or writes a SQLite store
#[test]
fn sqlite_fixtures_rows_identical() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/opencode");
    let Ok(rd) = std::fs::read_dir(&root) else { return };
    for case in rd.filter_map(|e| e.ok().map(|e| e.path())) {
        let native = case.join("native");
        let Some(db) =
            std::fs::read_dir(&native).ok().and_then(|r| r.filter_map(|e| e.ok().map(|e| e.path())).find(|p| p.extension().is_some_and(|x| x == "db")))
        else {
            continue;
        };
        let d = sqlite_self_trip_differences(&db);
        assert!(d.is_empty(), "{}: {d:?}", case.display());
    }
}

/// The 1.19.3 store's event log (234 rows) and project directory row, which
/// belong to no session model table, come back row for row.
#[cfg(feature = "sqlite")] // reads or writes a SQLite store
#[test]
fn v1_sqlite_1_19_3_store_self_trips_row_identical() {
    let d = sqlite_self_trip_differences(&fixture_db("v1-sqlite-1.19.3-generated"));
    assert!(d.is_empty(), "{d:?}");
}

/// The 2.0.31 store's instruction blobs and states, its worktree row and the
/// project's event-sequence watermark (a row that names no session) come back
/// row for row.
#[cfg(feature = "sqlite")] // reads or writes a SQLite store
#[test]
fn v2_sqlite_2_0_31_store_self_trips_row_identical() {
    let d = sqlite_self_trip_differences(&fixture_db("v2-sqlite-2.0.31-generated"));
    assert!(d.is_empty(), "{d:?}");
}

// ---------------------------------------------------------------- the store ledger

/// The two stores with rows that belong to no session (an event log's
/// project directory; instruction blobs, a worktree, a project watermark).
#[cfg(feature = "sqlite")] // used only by the SQLite tests
const STORE_CASES: [&str; 2] = ["v1-sqlite-1.19.3-generated", "v2-sqlite-2.0.31-generated"];

/// opencode's session ids descend: a session started later has a smaller id.
#[cfg(feature = "sqlite")] // used only by the SQLite tests
const NEWER_SESSION: &str = "ses_000000000000NewerSession01";
/// A project opened after the store's first one, whose id sorts first.
#[cfg(feature = "sqlite")] // used only by the SQLite tests
const LATER_PROJECT: &str = "0000000000000000000000000000000000000001";

#[cfg(feature = "sqlite")] // reads or writes a SQLite store
fn copy_fixture_db(case: &str, dir: &std::path::Path) -> std::path::PathBuf {
    let to = dir.join(case).join("opencode.db");
    std::fs::create_dir_all(to.parent().unwrap()).unwrap();
    std::fs::copy(fixture_db(case), &to).unwrap();
    to
}

#[cfg(feature = "sqlite")] // reads or writes a SQLite store
fn session_table(c: &rusqlite::Connection) -> &'static str {
    let v2: bool = c.query_row("SELECT count(*) FROM sqlite_master WHERE name = 'session_v2'", [], |r| r.get::<_, i64>(0)).unwrap() > 0;
    if v2 { "session_v2" } else { "session" }
}

/// What opencode does when it is opened in a directory it has not seen: a
/// project row (a copy of the first one, under `LATER_PROJECT`, created later).
#[cfg(feature = "sqlite")] // reads or writes a SQLite store
fn add_project(db: &std::path::Path) {
    let c = rusqlite::Connection::open(db).unwrap();
    c.execute_batch(&format!(
        "CREATE TEMP TABLE p AS SELECT * FROM project ORDER BY time_created LIMIT 1;
         UPDATE p SET id = '{LATER_PROJECT}', time_created = time_created + 600000;
         INSERT INTO project SELECT * FROM p; DROP TABLE p;"
    ))
    .unwrap();
}

/// A session started in `project` (a copy of the newest session, newer), with
/// its event-sequence watermark when the store keeps them.
#[cfg(feature = "sqlite")] // reads or writes a SQLite store
fn add_session(db: &std::path::Path, project: Option<&str>) {
    let c = rusqlite::Connection::open(db).unwrap();
    let t = session_table(&c);
    let project = project.map(|p| format!(", project_id = '{p}'")).unwrap_or_default();
    c.execute_batch(&format!(
        "CREATE TEMP TABLE s AS SELECT * FROM {t} ORDER BY time_created DESC LIMIT 1;
         UPDATE s SET id = '{NEWER_SESSION}', time_created = time_created + 1000, time_updated = time_updated + 1000{project};
         INSERT INTO {t} SELECT * FROM s; DROP TABLE s;"
    ))
    .unwrap();
    let has_seq: i64 = c.query_row("SELECT count(*) FROM sqlite_master WHERE name = 'event_sequence'", [], |r| r.get(0)).unwrap();
    if has_seq > 0 {
        c.execute(&format!("INSERT INTO event_sequence (aggregate_id, seq) VALUES ('{NEWER_SESSION}', 0)"), []).unwrap();
    }
}

/// The store ledger of an import, and every other ledger.
#[cfg(feature = "sqlite")] // used only by the SQLite tests
fn store_ledger_of(set: &LedgerSet) -> Option<&Ledger> {
    let mut stores = set.iter().filter(|l| is_store_ledger(l));
    let first = stores.next();
    assert!(stores.next().is_none(), "one store ledger per database");
    first
}

/// The store ledger holds only rows that belong to no session, has no parent
/// and no links, and every session ledger of the database names it with one
/// `member-of` link and never as its parent.
#[cfg(feature = "sqlite")] // reads a SQLite store
fn store_ledger_is_linked_member_of(case: &str) {
    let set = Opencode.import(&fixture_db(case), &ImportOptions::standard()).unwrap().set;
    let store = store_ledger_of(&set).unwrap_or_else(|| panic!("{case}: no store ledger"));
    assert!(store.agent().parent.is_none(), "{case}: the store ledger has a parent");
    assert!(store.agent().links.is_empty(), "{case}: the store ledger has links: {:?}", store.agent().links);
    assert!(!store.events.is_empty(), "{case}: an empty store ledger");
    for e in &store.events {
        let n = e.native.as_ref().unwrap_or_else(|| panic!("{case}: {} is not a native record", e.id));
        let (id, file) = (n.id.as_deref().unwrap_or(""), n.file.as_deref().unwrap_or(""));
        assert!(e.kind == Kind::Native && id.starts_with('@') && file.starts_with("opencode.db:"), "{case}: {} is not a row ({id}, {file})", e.id);
        let table = file.trim_start_matches("opencode.db:");
        assert!(!["session", "session_v2", "message", "session_message"].contains(&table), "{case}: a session model row in the store ledger: {id}");
    }
    let sessions: Vec<&Ledger> = set.iter().filter(|l| !is_store_ledger(l)).collect();
    assert!(!sessions.is_empty(), "{case}: no session ledger");
    for l in sessions {
        let to_store: Vec<&Link> = l.agent().links.iter().filter(|k| k.agent.as_deref() == Some(store.id())).collect();
        assert!(to_store.len() == 1 && to_store[0].rel == "member-of", "{case}: {} links to the store: {to_store:?}", l.id());
        assert!(l.agent().parent.as_ref().is_none_or(|p| p.agent != store.id()), "{case}: {} has the store ledger as parent", l.id());
    }
}

#[cfg(feature = "sqlite")] // reads a SQLite store
#[test]
fn v1_sqlite_session_ledgers_are_member_of_the_store_ledger() {
    store_ledger_is_linked_member_of(STORE_CASES[0]);
}

#[cfg(feature = "sqlite")] // reads a SQLite store
#[test]
fn v2_sqlite_session_ledgers_are_member_of_the_store_ledger() {
    store_ledger_is_linked_member_of(STORE_CASES[1]);
}

/// A session added to an attached opencode.db (the normal case: opencode's
/// ids descend, so the new session's id is the smallest) does not move a row
/// read earlier: every event of the first import is still there, under the
/// same agent, and the next sync is not diverged. The same holds when the
/// session is started in a project opencode wrote first with no session.
#[cfg(all(feature = "sqlite", feature = "sync"))] // attaches and syncs a store
fn session_added_to_attached_db(case: &str) {
    let dir = tempfile::tempdir().unwrap();
    let db = copy_fixture_db(case, dir.path());
    let mut s = crate::store::Store::open(dir.path().join("store")).unwrap();
    let (b, first) = crate::sync::attach(&mut s, "opencode", &db).unwrap();
    assert!(!first.appended.is_empty(), "{case}: the attach imported nothing");
    let before = Opencode.import(&db, &ImportOptions::standard()).unwrap().set;
    let bump = || std::thread::sleep(std::time::Duration::from_millis(20));

    // A project opened with no session yet, then a session started in it.
    bump();
    add_project(&db);
    let r = crate::sync::sync(&mut s, &b.binding).unwrap();
    assert!(r.diverged.is_none(), "{case}: after a project was added: {r:?}");
    bump();
    add_session(&db, Some(LATER_PROJECT));
    let r = crate::sync::sync(&mut s, &b.binding).unwrap();
    assert!(r.diverged.is_none(), "{case}: after a session was added: {:?}", r.diverged);
    assert_eq!(r.created, vec![NEWER_SESSION.to_string()], "{case}");

    // The import itself: every event read before is still under its agent.
    let after = Opencode.import(&db, &ImportOptions::standard()).unwrap().set;
    assert_ne!(before.len(), after.len(), "{case}: the added session is a ledger of its own");
    for l in before.iter() {
        let now = after.get(l.id()).unwrap_or_else(|| panic!("{case}: agent {} is gone", l.id()));
        let gone: Vec<&str> = l.events.iter().map(|e| e.id.as_str()).filter(|id| !now.contains(id)).collect();
        assert!(gone.is_empty(), "{case}: agent {}: {} events gone: {gone:?}", l.id(), gone.len());
    }
}

#[cfg(all(feature = "sqlite", feature = "sync"))] // attaches and syncs a store
#[test]
fn v1_sqlite_session_added_to_attached_db_syncs_without_divergence() {
    session_added_to_attached_db(STORE_CASES[0]);
}

#[cfg(all(feature = "sqlite", feature = "sync"))] // attaches and syncs a store
#[test]
fn v2_sqlite_session_added_to_attached_db_syncs_without_divergence() {
    session_added_to_attached_db(STORE_CASES[1]);
}

/// Every row id the store holds, with the number of ledgers holding it.
#[cfg(all(feature = "sqlite", feature = "store"))] // reads a store
fn row_holders(s: &crate::store::Store) -> (LedgerSet, std::collections::BTreeMap<String, usize>) {
    let mut set = LedgerSet::new();
    let mut held: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for a in s.agents().unwrap() {
        let l = Ledger::read(&s.ledger_path(&a.id).unwrap()).unwrap();
        for e in &l.events {
            if let Some(n) = e.native.as_ref().filter(|n| n.part == 0 && n.id.as_deref().is_some_and(|i| i.starts_with('@')))
                && n.file.as_deref().is_some_and(|f| f.starts_with("opencode.db:"))
            {
                *held.entry(n.id.clone().unwrap()).or_default() += 1;
            }
        }
        set.insert(l);
    }
    (set, held)
}

/// Importing the same opencode.db again after a session was added keeps
/// every row exactly once across the store (no row in two ledgers), and the
/// whole store exports back to that database row for row.
#[cfg(all(feature = "sqlite", feature = "store"))] // imports into a store
fn reimport_after_session_added(case: &str) {
    let dir = tempfile::tempdir().unwrap();
    let db = copy_fixture_db(case, dir.path());
    let mut s = crate::store::Store::open(dir.path().join("store")).unwrap();
    s.import("opencode", &db).unwrap();
    add_session(&db, None);
    let again = s.import("opencode", &db).unwrap();
    assert_eq!(again.created, vec![NEWER_SESSION.to_string()], "{case}");

    let (set, held) = row_holders(&s);
    let twice: Vec<(&String, &usize)> = held.iter().filter(|(_, n)| **n != 1).collect();
    assert!(twice.is_empty(), "{case}: rows held by more than one ledger: {twice:?}");
    let d = export_differences(&set, &db);
    assert!(d.is_empty(), "{case}: {d:?}");
    let store_rows = store_ledger_of(&set).map(|l| l.events.len()).unwrap_or(0);
    assert!(store_rows > 0, "{case}: no store ledger holds the rows that belong to no session");
}

#[cfg(all(feature = "sqlite", feature = "store"))] // imports into a store
#[test]
fn v1_sqlite_reimport_after_a_session_is_added_holds_each_row_once_and_exports() {
    reimport_after_session_added(STORE_CASES[0]);
}

#[cfg(all(feature = "sqlite", feature = "store"))] // imports into a store
#[test]
fn v2_sqlite_reimport_after_a_session_is_added_holds_each_row_once_and_exports() {
    reimport_after_session_added(STORE_CASES[1]);
}

/// A copy of `db` with every table's rows inserted in reverse rowid order,
/// then vacuumed.
#[cfg(feature = "sqlite")] // reads or writes a SQLite store
fn reversed_copy(db: &std::path::Path, to: &std::path::Path) {
    let c = rusqlite::Connection::open(to).unwrap();
    // Children before parents, in reverse: the copy does not check references.
    c.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
    let src = rusqlite::Connection::open(db).unwrap();
    let mut st = src.prepare("SELECT name, sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY rowid").unwrap();
    let objs: Vec<(String, String)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|r| r.unwrap()).collect();
    for (_, sql) in &objs {
        c.execute_batch(sql).unwrap();
    }
    c.execute(&format!("ATTACH DATABASE '{}' AS src", db.display()), []).unwrap();
    let tables: Vec<String> = {
        let mut st = src.prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY rowid").unwrap();
        st.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
    };
    for t in tables {
        c.execute(&format!("INSERT INTO main.\"{t}\" SELECT * FROM src.\"{t}\" ORDER BY rowid DESC"), []).unwrap();
    }
    c.execute_batch("DETACH DATABASE src; VACUUM;").unwrap();
}

#[cfg(feature = "sqlite")] // reads a SQLite store
fn store_id_of(db: &std::path::Path) -> String {
    let set = Opencode.import(db, &ImportOptions::standard()).unwrap().set;
    store_ledger_of(&set).unwrap_or_else(|| panic!("{}: no store ledger", db.display())).id().to_string()
}

/// The store ledger's agent id is a property of the database: the same for
/// a copy whose rows were written in the reverse order and vacuumed, the same
/// after a session (in a project opened later) is added, and different for
/// another database.
#[cfg(feature = "sqlite")] // reads or writes a SQLite store
#[test]
fn store_ledger_id_survives_row_order_vacuum_and_new_sessions() {
    let mut ids = Vec::new();
    for case in STORE_CASES {
        let dir = tempfile::tempdir().unwrap();
        let db = copy_fixture_db(case, dir.path());
        add_project(&db);
        let rev = dir.path().join("reversed").join("opencode.db");
        std::fs::create_dir_all(rev.parent().unwrap()).unwrap();
        reversed_copy(&db, &rev);
        // The two databases really do keep their rows in different orders.
        let first_by_rowid = |p: &std::path::Path| -> String {
            rusqlite::Connection::open(p).unwrap().query_row("SELECT id FROM project ORDER BY rowid LIMIT 1", [], |r| r.get(0)).unwrap()
        };
        assert_ne!(first_by_rowid(&db), first_by_rowid(&rev), "{case}: the reversed copy kept the row order");
        assert!(table_differences(&db, &rev).is_empty(), "{case}: the reversed copy holds other rows");

        let id = store_id_of(&db);
        assert_eq!(store_id_of(&rev), id, "{case}: reversed and vacuumed");
        add_session(&db, Some(LATER_PROJECT));
        assert_eq!(store_id_of(&db), id, "{case}: after a session was added");
        ids.push(id);
    }
    assert_ne!(ids[0], ids[1], "two databases share a store ledger id");
}

/// A copy of a fixture database with every session and every row naming one
/// deleted.
#[cfg(feature = "sqlite")] // reads or writes a SQLite store
fn copy_without_sessions(case: &str, dir: &std::path::Path) -> std::path::PathBuf {
    let db = copy_fixture_db(case, dir);
    {
        let c = rusqlite::Connection::open(&db).unwrap();
        let t = session_table(&c);
        let sids: Vec<String> = {
            let mut st = c.prepare(&format!("SELECT id FROM {t}")).unwrap();
            st.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
        };
        let tables: Vec<String> = {
            let mut st = c.prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'").unwrap();
            st.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
        };
        for tb in tables {
            let cols: Vec<String> = {
                let mut st = c.prepare(&format!("SELECT name FROM pragma_table_info('{tb}')")).unwrap();
                st.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
            };
            for col in ["session_id", "aggregate_id"].into_iter().filter(|k| cols.iter().any(|c| c == k)) {
                for sid in &sids {
                    c.execute(&format!("DELETE FROM \"{tb}\" WHERE {col} = ?1"), [sid]).unwrap();
                }
            }
        }
        c.execute(&format!("DELETE FROM {t}"), []).unwrap();
    }
    db
}

/// A database that holds no session at all: its rows go to the store ledger
/// (no import loss), and it exports back row for row.
#[cfg(feature = "sqlite")] // reads or writes a SQLite store
fn no_session_db(case: &str) {
    let dir = tempfile::tempdir().unwrap();
    let db = copy_without_sessions(case, dir.path());
    let imported = Opencode.import(&db, &ImportOptions::standard()).unwrap();
    assert_eq!(imported.losses.0.len(), 0, "{case}: {:?}", imported.losses.0);
    assert_eq!(imported.set.len(), 1, "{case}: the store ledger and nothing else");
    let store = store_ledger_of(&imported.set).unwrap_or_else(|| panic!("{case}: no store ledger"));
    assert!(!store.events.is_empty(), "{case}");
    let d = export_differences(&imported.set, &db);
    assert!(d.is_empty(), "{case}: {d:?}");
}

#[cfg(feature = "sqlite")] // reads or writes a SQLite store
#[test]
fn v1_sqlite_db_with_no_session_keeps_every_row_in_its_store_ledger() {
    no_session_db(STORE_CASES[0]);
}

#[cfg(feature = "sqlite")] // reads or writes a SQLite store
#[test]
fn v2_sqlite_db_with_no_session_keeps_every_row_in_its_store_ledger() {
    no_session_db(STORE_CASES[1]);
}

/// Attaching a database binds one of its sessions, never the store ledger
/// the sessions are `member-of`; a database with no session binds its store
/// ledger, the only ledger it has.
#[cfg(all(feature = "sqlite", feature = "sync"))] // attaches a store
fn attach_binds(case: &str) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = crate::store::Store::open(dir.path().join("store")).unwrap();
    let db = copy_fixture_db(case, dir.path());
    let set = Opencode.import(&db, &ImportOptions::standard()).unwrap().set;
    let store_id = store_ledger_of(&set).unwrap_or_else(|| panic!("{case}: no store ledger")).id().to_string();
    let (b, _) = crate::sync::attach(&mut s, "opencode", &db).unwrap();
    assert!(set.get(&b.agent).is_some_and(|l| !is_store_ledger(l)), "{case}: bound {} (store ledger {store_id})", b.agent);

    let bare = dir.path().join("bare");
    let db = copy_without_sessions(case, &bare);
    let (b, _) = crate::sync::attach(&mut s, "opencode", &db).unwrap();
    assert_eq!(b.agent, store_id, "{case}: a database with no session");
}

#[cfg(all(feature = "sqlite", feature = "sync"))] // attaches a store
#[test]
fn v1_sqlite_attach_binds_a_session_and_a_bare_db_its_store_ledger() {
    attach_binds(STORE_CASES[0]);
}

#[cfg(all(feature = "sqlite", feature = "sync"))] // attaches a store
#[test]
fn v2_sqlite_attach_binds_a_session_and_a_bare_db_its_store_ledger() {
    attach_binds(STORE_CASES[1]);
}

#[cfg(feature = "sqlite")] // used only by the SQLite tests
fn fixture_inputs() -> Vec<(String, std::path::PathBuf)> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/opencode");
    let mut out = Vec::new();
    for case in std::fs::read_dir(&root).into_iter().flatten().filter_map(|e| e.ok().map(|e| e.path())) {
        let native = case.join("native");
        if !native.is_dir() {
            continue;
        }
        let entries: Vec<_> = std::fs::read_dir(&native).into_iter().flatten().filter_map(|e| e.ok().map(|e| e.path())).collect();
        let input = if entries.len() == 1 && entries[0].is_file() { entries[0].clone() } else { native };
        out.push((case.file_name().unwrap().to_string_lossy().to_string(), input));
    }
    out.sort();
    out
}

/// Every fixture converts to every variant: with carry it comes back without
/// neutral loss, and the 2.x output passes the import endpoint's rules.
#[cfg(feature = "sqlite")] // reads or writes a SQLite store
#[test]
fn fixtures_convert_to_every_variant() {
    for (name, input) in fixture_inputs() {
        let p0 = Opencode.import(&input, &ImportOptions::standard()).unwrap().set;
        for v in ALL {
            for carry in [true, false] {
                let (back, out, losses) = export_import_losses(&p0, v, carry);
                // A container without a column for a field is a named loss even with carry.
                if carry && !losses.0.iter().any(|l| l.code == "column-dropped") {
                    // The store ledger's rows only go to a database of the
                    // container they came from (`native-dropped` elsewhere).
                    let d = compare(&without_store(&p0), &without_store(&back), CompareOptions { ignore_native: true, ..Default::default() });
                    assert!(d.is_clean(), "{name} via {v}: {}", d.summary());
                }
                if v == "v2-transfer" {
                    for (f, bytes) in &out.files {
                        let doc: Value = serde_json::from_slice(bytes).unwrap();
                        validate_transfer_tolerant(&doc).unwrap_or_else(|e| panic!("{name} via {v} (carry {carry}) {f}: {e}"));
                    }
                }
            }
        }
    }
}

/// `validate_transfer`, except for records copied verbatim from an opencode
/// source (their ids and shapes are opencode's own, including fork copies).
#[cfg(feature = "sqlite")] // used only by the SQLite tests
fn validate_transfer_tolerant(doc: &Value) -> Result<(), String> {
    let mut d = doc.clone();
    if let Some(ms) = d["messages"].as_array_mut() {
        ms.retain(|m| m.get("id").and_then(Value::as_str).is_some_and(|id| ids::is_canonical(id, "msg")));
    }
    if !s(&d["info"], "id").is_some_and(|i| ids::is_canonical(i, "ses")) {
        d["info"]["id"] = json!(ids::session(0, "x"));
    }
    validate_transfer(&d)
}

/// Helper for validating against a real opencode: compares two ledger
/// directories (`OPENCODE_COMPARE_A`, `OPENCODE_COMPARE_B`) the way the
/// conformance suite does. `cargo test --lib compare_dirs -- --ignored --nocapture`.
#[test]
#[ignore]
fn compare_dirs() {
    let (Ok(a), Ok(b)) = (std::env::var("OPENCODE_COMPARE_A"), std::env::var("OPENCODE_COMPARE_B")) else { return };
    let a = LedgerSet::read_path(std::path::Path::new(&a)).unwrap();
    let b = LedgerSet::read_path(std::path::Path::new(&b)).unwrap();
    let d = compare(&a, &b, CompareOptions { ignore_native: true, ..Default::default() });
    println!("COMPARE: {}", d.summary());
}

// ---------------------------------------------------------------- neutral mappings (SPEC §3.1)

/// One foreign ledger through `v2-transfer` without carry: the re-imported
/// ledger and the transfer document written for it.
fn v2_trip(events: Vec<Event>) -> (Ledger, Value) {
    let mut l = Ledger::new(agent("solo"));
    for e in events {
        l.append(e).unwrap();
    }
    let mut set = LedgerSet::new();
    set.insert(l);
    let (back, out) = export_import(&set, "v2-transfer", false);
    let doc: Value = serde_json::from_slice(out.files.values().next().unwrap()).unwrap();
    validate_transfer(&doc).unwrap();
    (back.iter().next().unwrap().clone(), doc)
}

fn kinds(l: &Ledger) -> Vec<&'static str> {
    l.events.iter().filter(|e| e.kind != Kind::Native).map(|e| e.kind.as_str()).collect()
}

fn of(l: &Ledger, kind: Kind) -> Vec<&Event> {
    l.events.iter().filter(|e| e.kind == kind).collect()
}

fn assistants(doc: &Value) -> Vec<&Value> {
    doc["messages"].as_array().unwrap().iter().filter(|m| s(m, "type") == Some("assistant")).collect()
}

#[test]
fn usage_output_includes_reasoning_and_excludes_it_in_opencode() {
    let (l, doc) = v2_trip(vec![
        ev("s", Kind::Settings, "2026-05-01T10:00:00.000Z", "s").with("model", "gpt-5"),
        ev("u", Kind::Text, "2026-05-01T10:00:01.000Z", "u").with("role", "user").with("text", "hi"),
        ev("a", Kind::Text, "2026-05-01T10:00:02.000Z", "r1").with("role", "assistant").with("text", "hello"),
        ev("k", Kind::Usage, "2026-05-01T10:00:02.000Z", "r1")
            .with("input", 100)
            .with("output", 50)
            .with("reasoning", 20)
            .with("cache_read", 7)
            .with("cache_write", 0),
        ev("t", Kind::Usage, "2026-05-01T10:00:03.000Z", "x")
            .with("input", 150)
            .with("output", 80)
            .with("reasoning", 20)
            .with("cumulative", true)
            .with("model", "gpt-5"),
    ]);
    // 2.x stores visible output only (packages/core/src/session/usage.ts).
    let tok = &assistants(&doc)[0]["tokens"];
    assert_eq!((tok["output"].as_i64(), tok["reasoning"].as_i64(), tok["input"].as_i64()), (Some(30), Some(20), Some(100)));
    assert_eq!(doc["info"]["tokens"]["output"].as_i64(), Some(60));
    let u = of(&l, Kind::Usage);
    assert_eq!((u[0].body["output"].as_i64(), u[0].body["reasoning"].as_i64(), u[0].body["cache_read"].as_i64()), (Some(50), Some(20), Some(7)));
    assert_eq!(u[0].body["cache_write"].as_i64(), Some(0), "a stated zero is kept");
    assert_eq!(u[0].str("model"), Some("gpt-5"));
    // Session totals that differ from the sum read back as a cumulative usage.
    let total = u.last().unwrap();
    assert_eq!(total.body["cumulative"], json!(true));
    assert_eq!((total.body["input"].as_i64(), total.body["output"].as_i64()), (Some(150), Some(80)));
}

#[test]
fn one_step_per_response() {
    let (l, doc) = v2_trip(vec![
        ev("u", Kind::Text, "2026-05-01T10:00:00.000Z", "u").with("role", "user").with("text", "go"),
        // Ungrouped: call, usage, result, then the next response.
        Event::new("c1", Kind::ToolCall).ts(Some("2026-05-01T10:00:01.000Z")).with("call_id", "c1").with("name", "ls").with("input", json!({})),
        Event::new("k1", Kind::Usage).ts(Some("2026-05-01T10:00:01.000Z")).with("input", 10).with("output", 5),
        Event::new("r1", Kind::ToolResult).ts(Some("2026-05-01T10:00:02.000Z")).with("call_id", "c1").with("output", "a b"),
        // The same usage reported twice for one response is written once.
        Event::new("k1b", Kind::Usage).ts(Some("2026-05-01T10:00:02.000Z")).with("input", 10).with("output", 5),
        ev("t2", Kind::Text, "2026-05-01T10:00:03.000Z", "g2").with("role", "assistant").with("text", "done"),
        // A usage that comes before its response's events opens that response's step.
        ev("k2", Kind::Usage, "2026-05-01T10:00:04.000Z", "g3").with("input", 30).with("output", 3).with("model", "gemini-2.5-pro"),
        ev("c3", Kind::ToolCall, "2026-05-01T10:00:04.000Z", "g3").with("call_id", "c3").with("name", "read").with("input", json!({"p": 1})),
        Event::new("r3", Kind::ToolResult).ts(Some("2026-05-01T10:00:05.000Z")).with("call_id", "c3").with("output", "x"),
    ]);
    let steps = assistants(&doc);
    assert_eq!(steps.len(), 3, "{doc:#}");
    assert_eq!(steps[0]["tokens"]["input"].as_i64(), Some(10));
    assert_eq!(steps[1]["content"][0]["type"], "text");
    assert!(steps[1].get("tokens").is_none());
    assert_eq!(steps[2]["tokens"]["input"].as_i64(), Some(30));
    // The usage names the model that billed it.
    assert_eq!(s(&steps[2]["model"], "id"), Some("gemini-2.5-pro"));
    assert_eq!(s(&steps[2]["model"], "providerID"), Some("google"));
    assert_eq!(of(&l, Kind::Usage).len(), 2);
    // The usage sits where the response finished streaming: after its own
    // events, before the tool result that came back later (SPEC §3.1.1).
    assert_eq!(steps[0]["time"]["streamed"].as_i64(), Some(crate::util::parse_rfc3339_ms("2026-05-01T10:00:01.000Z").unwrap()));
    let first: Vec<&str> = l.events.iter().filter(|e| e.kind != Kind::Native).skip(2).take(3).map(|e| e.kind.as_str()).collect();
    assert_eq!(first, ["tool_call", "usage", "tool_result"]);
}

#[test]
fn settings_switches_and_the_opening_snapshot() {
    let (l, doc) = v2_trip(vec![
        ev("s0", Kind::Settings, "2026-05-01T10:00:00.000Z", "s").with("provider", "my-gateway").with("cwd", "C:\\work\\repo"),
        ev("s1", Kind::Settings, "2026-05-01T10:00:00.100Z", "s").with("model", "claude-sonnet-4-5").with("agent", "build"),
        ev("u", Kind::Text, "2026-05-01T10:00:01.000Z", "u").with("role", "user").with("text", "hi"),
        ev("s2", Kind::Settings, "2026-05-01T10:00:02.000Z", "s").with("effort", "high"),
        ev("s3", Kind::Settings, "2026-05-01T10:00:03.000Z", "s").with("model", "gpt-5").with("provider", "openai"),
        ev("s4", Kind::Settings, "2026-05-01T10:00:04.000Z", "s").with("cwd", "relative/dir"),
    ]);
    // The session holds the current selection; each switch records the previous one.
    assert_eq!(s(&doc["info"]["model"], "id"), Some("gpt-5"));
    assert_eq!(s(&doc["info"]["location"], "directory"), Some("C:\\work\\repo"));
    let switches: Vec<&Value> = doc["messages"].as_array().unwrap().iter().filter(|m| s(m, "type") == Some("model-switched")).collect();
    assert_eq!(switches.len(), 2);
    assert_eq!(s(&switches[0]["model"], "variant"), Some("high"), "an effort change is a model variant");
    assert_eq!(s(&switches[0]["previous"], "providerID"), Some("my-gateway"), "a provider stated before the model is kept");
    // A switch that changes the model records the selection it replaced, not the new one.
    assert_eq!(switches[1]["model"], json!({"id": "gpt-5", "providerID": "openai", "variant": "high"}));
    assert_eq!(switches[1]["previous"], json!({"id": "claude-sonnet-4-5", "providerID": "my-gateway", "variant": "high"}));
    let set = of(&l, Kind::Settings);
    assert_eq!(set[0].str("model"), Some("claude-sonnet-4-5"), "the opening model is the first switch's previous");
    assert_eq!(set[0].str("provider"), Some("my-gateway"));
    assert_eq!(set[0].str("cwd"), Some("C:\\work\\repo"));
    assert_eq!((set[1].str("effort"), set[2].str("model")), (Some("high"), Some("gpt-5")));
}

#[test]
fn unknown_placeholders_read_back_as_unknown() {
    let (l, doc) = v2_trip(vec![
        ev("u", Kind::Text, "2026-05-01T10:00:01.000Z", "u").with("role", "user").with("text", "hi"),
        ev("a", Kind::Text, "2026-05-01T10:00:02.000Z", "r").with("role", "assistant").with("text", "hello"),
    ]);
    assert_eq!(s(&doc["info"]["location"], "directory"), Some("/"));
    assert_eq!(s(&assistants(&doc)[0]["model"], "providerID"), Some("unknown"));
    let set = of(&l, Kind::Settings);
    assert_eq!(set.len(), 1);
    assert!(set[0].body.is_empty(), "{:?}", set[0].body);
    // A model with no recorded provider is written with the `unknown` provider,
    // which reads back as no provider; the model itself is kept.
    let (l, doc) = v2_trip(vec![
        ev("s", Kind::Settings, "2026-05-01T10:00:00.000Z", "s").with("model", "local-llm"),
        ev("u", Kind::Text, "2026-05-01T10:00:01.000Z", "u").with("role", "user").with("text", "hi"),
    ]);
    assert_eq!(doc["info"]["model"], json!({"id": "local-llm", "providerID": "unknown"}));
    let set = of(&l, Kind::Settings);
    assert_eq!((set[0].str("model"), set[0].str("provider")), (Some("local-llm"), None));
}

#[test]
fn reasoning_seals_and_summaries() {
    let (l, doc) = v2_trip(vec![
        ev("u", Kind::Text, "2026-05-01T10:00:00.000Z", "u").with("role", "user").with("text", "hi"),
        // OpenAI: summary-only reasoning with encrypted content.
        ev("r1", Kind::Reasoning, "2026-05-01T10:00:01.000Z", "a1")
            .with("summary", json!(["Plan"]))
            .with("sealed", json!({"vendor": "openai", "kind": "encrypted", "data": "gAAA"})),
        ev("t1", Kind::Text, "2026-05-01T10:00:01.500Z", "a1").with("role", "assistant").with("text", "ok"),
        ev("u2", Kind::Text, "2026-05-01T10:00:02.000Z", "u2").with("role", "user").with("text", "more"),
        // Anthropic: redacted thinking; a signature over visible thinking.
        ev("r2", Kind::Reasoning, "2026-05-01T10:00:03.000Z", "a2")
            .with("text", "")
            .with("redacted", true)
            .with("sealed", json!({"vendor": "anthropic", "kind": "encrypted", "data": "EqQB"})),
        ev("r3", Kind::Reasoning, "2026-05-01T10:00:04.000Z", "a2")
            .with("text", "think")
            .with("sealed", json!({"vendor": "anthropic", "kind": "signature", "data": "sig"})),
        ev("t3", Kind::Text, "2026-05-01T10:00:06.000Z", "a2").with("role", "assistant").with("text", "answer"),
    ]);
    let steps = assistants(&doc);
    assert_eq!(steps[0]["content"][0]["state"], json!({"reasoningEncryptedContent": "gAAA"}));
    assert_eq!(s(&steps[0]["model"], "providerID"), Some("openai"), "the seal's vendor names an unknown provider");
    assert_eq!(steps[1]["content"][0]["state"], json!({"redactedData": "EqQB"}));
    assert_eq!(
        steps[1]["content"][1]["time"]["completed"].as_i64(),
        Some(crate::util::parse_rfc3339_ms("2026-05-01T10:00:06.000Z").unwrap()),
        "reasoning ends where the text begins"
    );
    let r = of(&l, Kind::Reasoning);
    assert_eq!(r[0].body.get("summary"), Some(&json!(["Plan"])));
    assert!(r[0].body.get("text").is_none());
    assert_eq!(r[0].body["sealed"], json!({"vendor": "openai", "kind": "encrypted", "data": "gAAA"}));
    assert_eq!(r[1].body["sealed"]["kind"], "encrypted");
    assert_eq!(r[2].body["sealed"]["kind"], "signature");
    let texts = of(&l, Kind::Text);
    assert_eq!(
        texts.last().unwrap().ts.as_deref().map(crate::util::norm_ts),
        Some(crate::util::norm_ts("2026-05-01T10:00:06.000Z")),
        "the text is dated by the reasoning before it"
    );
}

#[test]
fn summary_parts_of_one_openai_item_are_one_reasoning() {
    let m = json!({"id": "msg_1", "type": "assistant", "time": {"created": 1, "completed": 3}, "agent": "build", "model": {"id": "gpt-5", "providerID": "openai"},
        "content": [
            {"type": "reasoning", "text": "A", "state": {"itemId": "rs_1", "reasoningEncryptedContent": null}},
            {"type": "reasoning", "text": "B", "state": {"itemId": "rs_1", "reasoningEncryptedContent": "enc"}},
            {"type": "reasoning", "text": "", "state": {"itemId": "rs_2", "reasoningEncryptedContent": null}},
            {"type": "reasoning", "text": "plain", "state": {"thoughtSignature": "ts"}, "time": {"created": 2}}]});
    let evs = v2::assistant_events_for_test(&m);
    let r: Vec<&Event> = evs.iter().filter(|e| e.kind == Kind::Reasoning).collect();
    assert_eq!(r.len(), 3);
    assert_eq!(r[0].body["summary"], json!(["A", "B"]));
    assert_eq!(r[0].body["sealed"]["data"], "enc");
    assert_eq!(r[1].body["summary"], json!([]));
    assert_eq!(r[2].str("text"), Some("plain"));
    assert_eq!(r[2].body["sealed"], json!({"vendor": "google", "kind": "signature", "data": "ts"}), "the state key names the vendor");
}

#[test]
fn tool_inputs_and_unsettled_calls() {
    let (l, doc) = v2_trip(vec![
        ev("u", Kind::Text, "2026-05-01T10:00:00.000Z", "u").with("role", "user").with("text", "go"),
        ev("c1", Kind::ToolCall, "2026-05-01T10:00:01.000Z", "a")
            .with("call_id", "c1")
            .with("name", "apply_patch")
            .with("input_text", "*** Begin Patch")
            .with("tool_type", "custom"),
        ev("c2", Kind::ToolCall, "2026-05-01T10:00:01.000Z", "a").with("call_id", "c2").with("name", "run").with("input", "{\"command\":\"ls\"}"),
        ev("c3", Kind::ToolCall, "2026-05-01T10:00:01.000Z", "a").with("call_id", "c3").with("name", "say").with("input", "not json"),
        ev("c4", Kind::ToolCall, "2026-05-01T10:00:01.000Z", "a").with("call_id", "c4").with("name", "pick").with("input", json!(["a"])),
        ev("r1", Kind::ToolResult, "2026-05-01T10:00:02.000Z", "x").with("call_id", "c1").with("output", "fail").with("is_error", true),
    ]);
    let c = &assistants(&doc)[0]["content"];
    assert_eq!(c[0]["state"]["input"], json!({"input": "*** Begin Patch"}));
    assert_eq!(c[1]["state"]["input"], json!({"command": "ls"}), "JSON text arguments are written parsed");
    assert_eq!(c[2]["state"]["input"], json!({"value": "not json"}));
    assert_eq!(c[3]["state"]["error"]["message"], v2::UNSETTLED_MESSAGE);
    let calls = of(&l, Kind::ToolCall);
    assert_eq!((calls[0].str("input_text"), calls[0].str("tool_type")), (Some("*** Begin Patch"), Some("custom")));
    assert_eq!(calls[1].body["input"], json!({"command": "ls"}));
    assert_eq!(calls[3].body["input"], json!(["a"]));
    let results = of(&l, Kind::ToolResult);
    assert_eq!(results.len(), 1, "calls with no result read back without one");
    assert!(results[0].body.get("error").is_none(), "a tool.execution error restating the output is not repeated");
    assert_eq!(results[0].body["is_error"], json!(true));
}

#[test]
fn parallel_results_follow_completion_time() {
    let (l, _) = v2_trip(vec![
        ev("u", Kind::Text, "2026-05-01T10:00:00.000Z", "u").with("role", "user").with("text", "go"),
        ev("c1", Kind::ToolCall, "2026-05-01T10:00:01.000Z", "a").with("call_id", "c1").with("name", "Read").with("input", json!({})),
        ev("r1", Kind::ToolResult, "2026-05-01T10:00:01.100Z", "x").with("call_id", "c1").with("output", "1"),
        ev("c2", Kind::ToolCall, "2026-05-01T10:00:02.000Z", "a").with("call_id", "c2").with("name", "Bash").with("input", json!({})),
        ev("c3", Kind::ToolCall, "2026-05-01T10:00:03.000Z", "a").with("call_id", "c3").with("name", "Bash").with("input", json!({})),
        ev("r3", Kind::ToolResult, "2026-05-01T10:00:04.000Z", "x").with("call_id", "c3").with("output", "3"),
        ev("r2", Kind::ToolResult, "2026-05-01T10:00:05.000Z", "x").with("call_id", "c2").with("output", "2"),
    ]);
    let order: Vec<String> = l
        .events
        .iter()
        .filter(|e| matches!(e.kind, Kind::ToolCall | Kind::ToolResult))
        .map(|e| format!("{}:{}", e.kind.as_str(), e.str("call_id").unwrap()))
        .collect();
    assert_eq!(order, ["tool_call:c1", "tool_result:c1", "tool_call:c2", "tool_call:c3", "tool_result:c3", "tool_result:c2"]);
}

#[test]
fn failed_request_is_an_empty_step_with_its_error() {
    let (l, doc) = v2_trip(vec![
        ev("u", Kind::Text, "2026-05-01T10:00:00.000Z", "u").with("role", "user").with("text", "hi"),
        ev("e", Kind::Error, "2026-05-01T10:00:01.000Z", "e").with("kind", "billing_error").with("message", "Credit balance is too low").with("status", 400),
        ev("a", Kind::Text, "2026-05-01T10:00:02.000Z", "r").with("role", "assistant").with("text", "later"),
    ]);
    let steps = assistants(&doc);
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0]["content"], json!([]));
    assert_eq!(steps[0]["error"], json!({"type": "billing_error", "message": "Credit balance is too low", "status": 400}));
    assert_eq!(kinds(&l), ["settings", "text", "error", "text"]);
    // Ungrouped, nothing but the error says the response is over: the reply that
    // follows is still another step, not content of the failed one.
    let (_, doc) = v2_trip(vec![
        ev("u", Kind::Text, "2026-05-01T10:00:00.000Z", "u").with("role", "user").with("text", "hi"),
        Event::new("e", Kind::Error).ts(Some("2026-05-01T10:00:01.000Z")).with("kind", "billing_error").with("message", "Credit balance is too low"),
        Event::new("a", Kind::Text).ts(Some("2026-05-01T10:00:02.000Z")).with("role", "assistant").with("text", "later"),
    ]);
    let steps = assistants(&doc);
    assert_eq!(steps.len(), 2, "{doc:#}");
    assert_eq!((steps[0]["content"].clone(), steps[1]["content"][0]["text"].clone()), (json!([]), json!("later")));
}

#[test]
fn record_only_text_is_not_shown_to_the_model() {
    let mut note = ev("n", Kind::Text, "2026-05-01T10:00:01.000Z", "n").with("role", "user").with("text", "UI notice");
    note.audience = Some(crate::model::Audience::Record);
    let events = vec![ev("u", Kind::Text, "2026-05-01T10:00:00.000Z", "u").with("role", "user").with("text", "hi"), note];
    let (l, doc) = v2_trip(events.clone());
    assert!(!serde_json::to_string(&doc).unwrap().contains("UI notice"));
    assert_eq!(of(&l, Kind::Text).len(), 1);
    // 1.x keeps it as an ignored text part, read back as record-only text.
    let mut v1 = Ledger::new(agent("solo"));
    for e in events {
        v1.append(e).unwrap();
    }
    let mut set = LedgerSet::new();
    set.insert(v1);
    let (back, _) = export_import(&set, "v1-export", false);
    let t = back.iter().next().unwrap().events.iter().find(|e| e.str("text") == Some("UI notice")).cloned().unwrap();
    assert_eq!(t.effective_audience(), crate::model::Audience::Record);
}

#[test]
fn opening_settings_come_from_the_first_switch() {
    // A 2.x session's agent/model/location are the current selection.
    let info = json!({"id": "ses_x", "model": {"id": "b", "providerID": "p2"}, "agent": "plan", "location": {"directory": "/now"}, "time": {"created": 1}});
    let msgs = [
        json!({"type": "agent-switched", "agent": "plan", "previous": "build"}),
        json!({"type": "model-switched", "model": {"id": "b", "providerID": "p2"}, "previous": {"id": "a", "providerID": "p1", "variant": "low"}}),
        json!({"type": "location-switched", "location": {"directory": "/now"}, "previous": {"location": {"directory": "/then"}}}),
    ];
    let set = v2::session_events(&info, msgs.iter())[0].clone();
    assert_eq!(
        (set.str("agent"), set.str("model"), set.str("provider"), set.str("effort"), set.str("cwd")),
        (Some("build"), Some("a"), Some("p1"), Some("low"), Some("/then"))
    );
    // A switch with no previous value: nothing was selected before it.
    let msgs = [json!({"type": "model-switched", "model": {"id": "b", "providerID": "p2"}})];
    let set = v2::session_events(&info, msgs.iter())[0].clone();
    assert!(set.str("model").is_none());
    assert_eq!(set.str("agent"), Some("plan"));
}

#[test]
fn v1_output_convention_follows_the_version() {
    assert!(!v1::output_excludes_reasoning(Some("1.2.21")));
    assert!(!v1::output_excludes_reasoning(Some("1.1.8")));
    assert!(v1::output_excludes_reasoning(Some("1.19.3")));
    let mut l = Ledger::new(agent("solo"));
    for e in [
        ev("u", Kind::Text, "2026-05-01T10:00:00.000Z", "u").with("role", "user").with("text", "hi"),
        ev("a", Kind::Text, "2026-05-01T10:00:01.000Z", "r").with("role", "assistant").with("text", "hello"),
        ev("k", Kind::Usage, "2026-05-01T10:00:01.000Z", "r").with("input", 10).with("output", 50).with("reasoning", 20),
    ] {
        l.append(e).unwrap();
    }
    let mut set = LedgerSet::new();
    set.insert(l);
    for (version, stored) in [("1.2.21", 50), ("1.19.3", 30)] {
        let out = Opencode.export(&set, &ExportOptions { variant: Some("v1-export".into()), carry: false, target_version: Some(version.into()) }).unwrap();
        let doc: Value = serde_json::from_slice(out.output.files.values().next().unwrap()).unwrap();
        let tok = doc["messages"].as_array().unwrap().iter().find(|m| m["info"]["role"] == "assistant").unwrap()["info"]["tokens"].clone();
        assert_eq!(tok["output"].as_i64(), Some(stored), "{version}");
        let dir = tempfile::tempdir().unwrap();
        let p = materialise(&out.output, dir.path()).unwrap();
        let back = Opencode.import(&p, &ImportOptions { variant: None, restore_carried: false }).unwrap().set;
        let u = back.iter().next().unwrap().events.iter().find(|e| e.kind == Kind::Usage).cloned().unwrap();
        assert_eq!(u.body["output"].as_i64(), Some(50), "{version}");
    }
}

#[test]
fn peer_messages_use_the_spec_wrapper() {
    let peer = ev("p", Kind::PeerMessage, "2026-05-01T10:00:01.000Z", "p")
        .with("direction", "in")
        .with("peer", "agent-7")
        .with("network", "mesh")
        .with("text", "hello\nthere");
    let (l, doc) = v2_trip(vec![ev("u", Kind::Text, "2026-05-01T10:00:00.000Z", "u").with("role", "user").with("text", "hi"), peer.clone()]);
    let m = doc["messages"].as_array().unwrap().iter().find(|m| s(m, "type") == Some("synthetic")).unwrap();
    assert_eq!(s(m, "text"), Some("<peer-message direction=\"in\" peer=\"agent-7\" network=\"mesh\">\nhello\nthere\n</peer-message>"));
    let p = of(&l, Kind::PeerMessage);
    assert_eq!((p[0].str("peer"), p[0].str("network"), p[0].str("text")), (Some("agent-7"), Some("mesh"), Some("hello\nthere")));
    assert!(peer_unwrap("<peer-message>\nx\n</peer-message>").is_none(), "a direction is required");
    // 1.x: a synthetic text part, read back the same way.
    let mut v1 = Ledger::new(agent("solo"));
    v1.append(peer).unwrap();
    let mut set = LedgerSet::new();
    set.insert(v1);
    let (back, _) = export_import(&set, "v1-export", false);
    assert_eq!(back.iter().next().unwrap().events.iter().filter(|e| e.kind == Kind::PeerMessage).count(), 1);
}

/// `opencode export` (1.x) and `opencode session export` (2.x) documents whose
/// records are spelled as no parsed value re-spells them (unsorted keys, `1e3`,
/// `1.50`, the escape `\u00e9`) come back byte for byte: each record is held
/// as its own text and the document is laid out again from those texts.
const ODD_EXPORT: &str = r#"{"info":{"id":"oc-odd","directory":"/w","time":{"created":1767312000000}},"messages":[{"info":{"id":"oc-odd-user","role":"user","time":{"created":1767312001000}},"parts":[{"type":"text","text":"Read the caf\u00e9 file."}]},{"info":{"id":"oc-odd-assistant","role":"assistant","modelID":"gpt-test","time":{"created":1767312002000}},"parts":[{"type":"step-start"},{"type":"future-semantic-part","value":"ignored","weight":1e3,"ratio":1.50},{"type":"text","text":"ol\u00e1"},{"type":"step-finish"}]}]}"#;
const ODD_TRANSFER: &str = r#"{"info":{"id":"ses_odd","title":"caf\u00e9","time":{"created":1792195332259,"updated":1792195405180},"location":{"directory":"/w"},"ratio":1.50},"messages":[{"id":"msg_odd1","time":{"created":1792195332334},"text":"caf\u00e9","files":[],"type":"user","weight":1e3}]}"#;

#[test]
fn export_documents_reemit_odd_spellings_exactly() {
    for (name, compact) in [("session.json", ODD_EXPORT), ("ses_odd.json", ODD_TRANSFER)] {
        let text = crate::util::pretty(compact) + "\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, &text).unwrap();
        let imp = Opencode.import(&path, &ImportOptions::standard()).unwrap();
        assert!(!imp.losses.0.iter().any(|l| l.code == "export-file-reformatted"), "{name}: {:?}", imp.losses);
        let st = crate::roundtrip::self_trip(&Opencode, &path).unwrap();
        assert!(st.files.is_exact(), "{name}: self trip not exact: differ {:?} missing {:?} extra {:?}", st.files.differ, st.files.missing, st.files.extra);
        for via in ["claude-code", "codex"] {
            let ct = crate::roundtrip::cross_trip(&Opencode, &path, crate::formats::by_name(via).unwrap(), true).unwrap();
            assert!(ct.back_files.is_exact(), "{name}: carry trip via {via} not exact: differ {:?}", ct.back_files.differ);
        }
    }
}

/// A foreign native record spelled as no `Value` spells it: `1.50` and
/// `0.000005` (serde writes `1.5` and `5e-6`).
#[cfg(feature = "sqlite")] // used only by the SQLite tests
const ODD_RECORD: &str = r#"{"type":"assistant","cost":1.50,"tiny":0.000005}"#;

#[cfg(feature = "sqlite")] // used only by the SQLite tests
fn odd_native(e: Event) -> Event {
    let id = e.id.clone();
    e.native(crate::model::Native::new("claude-code", Some(id)).record_raw(ODD_RECORD).unwrap())
}

/// The native record text of each event in `ids`, as `back` holds it.
#[cfg(feature = "sqlite")] // used only by the SQLite tests
fn record_texts(back: &LedgerSet, agent: &str, ids: &[&str]) -> Vec<Option<String>> {
    let l = back.get(agent).unwrap_or_else(|| panic!("{agent} came back"));
    ids.iter().map(|id| l.events.iter().find(|e| e.id == *id).and_then(|e| e.native.as_ref()).and_then(|n| n.record_text()).map(str::to_string)).collect()
}

#[cfg(feature = "sqlite")] // reads or writes a SQLite store
#[test]
fn carried_native_records_restore_their_source_text() {
    // Sentinel: a `Value` re-spells the record, so only its carried text can
    // bring it back.
    let v: Value = serde_json::from_str(ODD_RECORD).unwrap();
    assert_ne!(serde_json::to_string(&v).unwrap(), ODD_RECORD);
    let want = vec![Some(ODD_RECORD.to_string()); 3];
    // On messages, and after the last message.
    let mut set = foreign_set();
    {
        let p = set.ledgers.get_mut("parent-agent").unwrap();
        for e in p.events.iter_mut().filter(|e| e.id == "e8" || e.id == "e9") {
            *e = odd_native(e.clone());
        }
        p.append(odd_native(ev("e14", Kind::Native, "2026-05-01T10:00:14.000Z", "n"))).unwrap();
    }
    // With no message at all: on the session.
    let mut lone = Ledger::new(agent("lone-agent"));
    lone.append(ev("t1", Kind::Title, "2026-05-01T10:00:00.000Z", "t").with("text", "Nothing said")).unwrap();
    for id in ["n1", "n2", "n3"] {
        lone.append(odd_native(ev(id, Kind::Native, "2026-05-01T10:00:01.000Z", "n"))).unwrap();
    }
    let lone = LedgerSet::single(lone);
    for v in ALL {
        let (back, _) = export_import(&set, v, true);
        // Compared with the source's bytes, not with a re-serialised `Value`.
        assert_eq!(record_texts(&back, "parent-agent", &["e8", "e9", "e14"]), want, "{v}: on messages");
        // With no message, a 1.x storage directory is not recognised as a
        // session, and a 1.x session row has no metadata to carry anything in.
        if v == "v1-storage-json" || v == "v1-sqlite" {
            continue;
        }
        let (back, _) = export_import(&lone, v, true);
        assert_eq!(record_texts(&back, "lone-agent", &["n1", "n2", "n3"]), want, "{v}: on the session");
    }
}

// ---- branches (SPEC §2.3) ----

fn fixture(rel: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(rel)
}

/// A real opencode export document with a staged revert added to its session
/// info, as `SessionRevert.stage` leaves it (2.x `{messageID}`; 1.x may add a
/// `partID`): no opencode-made session with a revert exists under
/// `fixtures/opencode/`. Returns the written file.
fn with_revert(rel: &str, revert: Value, dir: &std::path::Path) -> std::path::PathBuf {
    let src = fixture(rel);
    let text = std::fs::read_to_string(&src).unwrap();
    let r = serde_json::to_string_pretty(&revert).unwrap().replace('\n', "\n    ");
    let at = "\n  },\n  \"messages\": [";
    assert_eq!(text.matches(at).count(), 1);
    let out = text.replacen(at, &format!(",\n    \"revert\": {r}{at}"), 1);
    let path = dir.join(src.file_name().unwrap());
    std::fs::write(&path, out).unwrap();
    path
}

fn import_one(path: &std::path::Path) -> (Ledger, crate::formats::Losses) {
    let imp = Opencode.import(path, &ImportOptions::standard()).unwrap();
    (imp.set.iter().next().unwrap().clone(), imp.losses)
}

const V2_DOC: &str = "opencode/v2-transfer-2.0.31-generated/native/export-ses_eb8d458b3ffeZ1K0WtB1fzo7r8.json";
const V2_SECOND_PROMPT: &str = "msg_14736ca06001C73fPLNkOhbtPr";
const V1_DOC: &str = "opencode/v1-export-1.2.21-generated/native/export-ses_eb8d6b52fffeC2XYPO9qzTJrOG.json";
const V1_ANSWER: &str = "msg_1472a1dff001vYMii6NMellfSB";
const V1_ANSWER_TEXT: &str = "prt_1472a1e6b0018hEUID4FI8y0ys";
const V1_ANSWER_START: &str = "prt_1472a1e6a0014E0ysr3GhDiZ9u";

/// (c) A staged revert becomes an `after` branch: one more part of the
/// session record, after the messages, continuing from the event before the
/// reverted message. Every reverted event stays; the document comes back byte
/// for byte.
#[test]
fn a_staged_revert_is_an_after_branch_that_keeps_the_reverted_messages() {
    let dir = tempfile::tempdir().unwrap();
    let (original, _) = import_one(&fixture(V2_DOC));
    let path = with_revert(V2_DOC, json!({"messageID": V2_SECOND_PROMPT}), dir.path());
    let (l, _) = import_one(&path);
    let sid = original.id().to_string();
    let m = l.events.iter().position(|e| e.native.as_ref().is_some_and(|n| n.id.as_deref() == Some(sid.as_str()) && n.part > 0) && e.after.is_some()).unwrap();
    // Every event is kept, unchanged but for the session record, which now
    // holds the revert (and positions, which the marker shifts).
    let same = |e: &Event| {
        let n = e.native.as_ref().filter(|n| n.id.as_deref() != Some(sid.as_str()));
        (e.id.clone(), e.kind, e.body.clone(), e.after.clone(), n.cloned())
    };
    let rest: Vec<_> = l.events.iter().enumerate().filter(|(i, _)| *i != m).map(|(_, e)| same(e)).collect();
    assert_eq!(rest, original.events.iter().map(same).collect::<Vec<_>>(), "every event is kept, unchanged");
    let first = l.events.iter().position(|e| e.native.as_ref().and_then(|n| n.id.as_deref()) == Some(V2_SECOND_PROMPT)).unwrap();
    assert!(first < m);
    assert_eq!(l.events[m].after.as_deref(), Some(l.events[first - 1].id.as_str()));
    let active: Vec<&str> = l.active_path().iter().map(|e| e.id.as_str()).collect();
    let mut want: Vec<&str> = l.events[..first].iter().map(|e| e.id.as_str()).collect();
    want.extend(l.events[m..].iter().map(|e| e.id.as_str()));
    assert_eq!(active, want);
    assert!(m - first >= 4, "the reverted messages are the abandoned branch");
    let st = crate::roundtrip::self_trip(&Opencode, &path).unwrap();
    assert!(st.files.is_exact(), "self trip not exact: differ {:?}", st.files.differ);
    for via in ["claude-code", "codex"] {
        let ct = crate::roundtrip::cross_trip(&Opencode, &path, crate::formats::by_name(via).unwrap(), true).unwrap();
        assert_eq!(ct.there.score(), 0, "carried trip via {via} lost something");
        assert!(ct.back_files.is_exact(), "carry trip via {via} not exact: differ {:?}", ct.back_files.differ);
    }
}

/// Opposes the test above: a 1.x revert with a `partID` keeps that message's
/// parts before the part; a revert naming no message the session holds is no
/// branch, and says so.
#[test]
fn a_part_revert_keeps_the_parts_before_it_and_an_unknown_one_is_no_branch() {
    let dir = tempfile::tempdir().unwrap();
    let path = with_revert(V1_DOC, json!({"messageID": V1_ANSWER, "partID": V1_ANSWER_TEXT}), dir.path());
    let (l, _) = import_one(&path);
    let at = |id: &str| l.events.iter().position(|e| e.native.as_ref().and_then(|n| n.id.as_deref()) == Some(id)).unwrap();
    let marker = l.events.iter().position(|e| e.after.is_some()).unwrap();
    assert_eq!(l.events[marker].after.as_deref(), Some(l.events[at(V1_ANSWER_TEXT) - 1].id.as_str()));
    let active: Vec<&str> = l.active_path().iter().map(|e| e.id.as_str()).collect();
    assert!(active.contains(&l.events[at(V1_ANSWER_START)].id.as_str()), "the message's earlier parts stay on the active path");
    assert!(!active.contains(&l.events[at(V1_ANSWER_TEXT)].id.as_str()));
    // A 1.x message's info (its usage) follows its parts, so it falls on the
    // abandoned side with the reverted part: a branch is a contiguous run.
    assert!(at(V1_ANSWER) > at(V1_ANSWER_TEXT) && !active.contains(&l.events[at(V1_ANSWER)].id.as_str()));
    let st = crate::roundtrip::self_trip(&Opencode, &path).unwrap();
    assert!(st.files.is_exact(), "self trip not exact: differ {:?}", st.files.differ);

    let path = with_revert(V2_DOC, json!({"messageID": "msg_not_in_this_session"}), dir.path());
    let (l, losses) = import_one(&path);
    assert!(!l.is_branched());
    assert!(losses.0.iter().any(|x| x.code == "revert-not-located"), "{losses:?}");
}

/// Every `text` a document renders, carriers (`metadata`, `protoledger`) aside.
fn doc_texts(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                match (k.as_str(), x) {
                    ("metadata" | "protoledger", _) => {}
                    ("text", Value::String(t)) => out.push(t.clone()),
                    _ => doc_texts(x, out),
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| doc_texts(x, out)),
        _ => {}
    }
}

/// (b) A foreign branched ledger exports its active path only; with carry the
/// abandoned events ride along and come back with their branch.
#[test]
fn a_foreign_branch_exports_only_its_active_path() {
    let set = LedgerSet::read_path(&fixture("protoledger/branch-rewind/native")).unwrap();
    for carry in [false, true] {
        let (back, out, losses) = export_import_losses(&set, "v2-transfer", carry);
        let doc: Value = serde_json::from_slice(out.files.values().next().unwrap()).unwrap();
        let mut said = vec![];
        doc_texts(&doc["messages"], &mut said);
        assert_eq!(
            said,
            [
                "Write a haiku about rain.",
                "Soft rain on the roof\ncounting every tired tile\nthe gutter hums low",
                "Never mind the count. Translate the haiku into French.",
                "Douce pluie sur le toit\ncomptant chaque tuile lasse\nla gouttière chante"
            ]
        );
        assert_eq!(losses.0.iter().find(|l| l.code == "abandoned-branch-not-rendered").map(|l| l.count), Some(4));
        assert!(!serde_json::to_string(&doc["messages"]).unwrap().contains("\"type\":\"tool\""), "the abandoned call was rendered");
        if carry {
            assert_eq!(back.iter().next().unwrap().events, set.iter().next().unwrap().events);
        }
    }
}

/// A record held as its source text keeps no second copy in `raw` —
/// in export documents (odd spellings included) and in a storage tree.
#[test]
fn a_record_held_as_its_text_keeps_no_raw() {
    let dir = tempfile::tempdir().unwrap();
    let mut inputs = vec![fixture("opencode/v1-storage-json-1.1.8-generated/native")];
    for (name, compact) in [("session.json", ODD_EXPORT), ("ses_odd.json", ODD_TRANSFER)] {
        let p = dir.path().join(name);
        std::fs::write(&p, crate::util::pretty(compact) + "\n").unwrap();
        inputs.push(p);
    }
    for path in inputs {
        let set = Opencode.import(&path, &ImportOptions::standard()).unwrap().set;
        let natives: Vec<&crate::model::Native> = set.iter().flat_map(|l| &l.events).filter_map(|e| e.native.as_ref()).filter(|n| n.part == 0).collect();
        assert!(natives.len() >= 2);
        assert!(natives.iter().all(|n| n.record_text().is_some()), "{}", path.display());
        assert_eq!(natives.iter().filter(|n| n.raw.is_some()).count(), 0, "{}: a record held as its exact text also kept a raw copy", path.display());
    }
}

/// Opposes the test above: a storage file laid out as no layout reproduces
/// from its record's text (tab-indented) still keeps its text in `raw`, and
/// comes back byte for byte.
#[test]
fn a_file_its_record_cannot_hold_keeps_raw() {
    let src = fixture("opencode/v1-storage-json-1.1.8-generated/native");
    let dir = tempfile::tempdir().unwrap();
    for e in walkdir::WalkDir::new(&src).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()) {
        let to = dir.path().join(e.path().strip_prefix(&src).unwrap());
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(e.path(), &to).unwrap();
    }
    let msg =
        walkdir::WalkDir::new(dir.path().join("storage/message")).into_iter().filter_map(|e| e.ok()).find(|e| e.file_type().is_file()).unwrap().into_path();
    let tabbed = std::fs::read_to_string(&msg).unwrap().replace("  ", "\t");
    std::fs::write(&msg, &tabbed).unwrap();
    let id = msg.file_stem().unwrap().to_str().unwrap().to_string();
    let set = Opencode.import(dir.path(), &ImportOptions::standard()).unwrap().set;
    let n = set.iter().flat_map(|l| &l.events).filter_map(|e| e.native.as_ref()).find(|n| n.part == 0 && n.id.as_deref() == Some(id.as_str())).unwrap();
    assert_eq!(n.raw.as_deref(), Some(tabbed.as_str()));
    let st = crate::roundtrip::self_trip(&Opencode, dir.path()).unwrap();
    assert!(st.files.is_exact(), "self trip not exact: differ {:?}", st.files.differ);
}

/// End to end: an opencode message whose `time.created` no id can
/// encode (a damaged file) raises the clock that events with no `ts` take, and
/// the event added after it still gets an id after every message id before
/// it; the export names `time-unrepresentable`. Opposed: the undamaged file
/// names nothing.
#[test]
fn a_damaged_message_time_neither_poisons_later_ids_nor_goes_unnamed() {
    let src =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/opencode/v1-export-1.19.3-generated/native/export-ses_eb8cd8da4ffetNuRJovyhykA9L.json");
    let text = std::fs::read_to_string(&src).unwrap();
    for damaged in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("export.json");
        let t = if damaged { text.replacen("\"created\": 1792195850864", "\"created\": 9000000000000000000", 1) } else { text.clone() };
        assert_eq!(t != text, damaged, "the damage was not planted");
        std::fs::write(&p, &t).unwrap();
        let mut set = Opencode.import(&p, &ImportOptions::standard()).unwrap().set;
        let id = set.iter().next().unwrap().id().to_string();
        let l = set.ledgers.get_mut(&id).unwrap();
        for k in 0..3 {
            l.append(Event::new(format!("added-{k}"), Kind::Text).with("role", "user").with("text", format!("added {k}"))).unwrap();
        }
        let out = Opencode.export(&set, &ExportOptions { variant: Some("v1-export".into()), carry: false, target_version: None }).unwrap();
        let named = out.losses.0.iter().find(|l| l.code == "time-unrepresentable").map(|l| l.count);
        assert_eq!(named.is_some(), damaged, "{:?}", out.losses.0);
        let doc: Value = serde_json::from_slice(out.output.files.values().next().unwrap()).unwrap();
        let ids: Vec<String> = doc["messages"].as_array().unwrap().iter().map(|m| m["info"]["id"].as_str().unwrap().to_string()).collect();
        let added = ids.len() - 3;
        assert!(ids[added..].windows(2).all(|w| w[0] < w[1]), "damaged={damaged}: the added messages are not in order: {:?}", &ids[added..]);
        assert!(ids[..added].iter().all(|m| *m < ids[added]), "damaged={damaged}: an added message sorts before an earlier one: {ids:?}");
    }
}
