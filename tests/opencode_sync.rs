//! Sync of a live opencode store (SPEC §4.1, §9.3), for each of the three
//! stores opencode writes in place: the 1.x storage tree, the 1.x
//! `opencode.db` and the 2.x `opencode.db`.
//!
//! A step's two tool calls are driven through their states — `pending` (1.x)
//! or `streaming` (2.x), then `running`, then `completed` for one and `error`
//! for the other — while a text grows and the step finishes; the store is
//! followed by a [`Follower`] polling every 200 ms after each transition. The
//! ledger must gain each call when its tool runs, each result when it
//! settled, the text when it ended and the usage when the step finished,
//! each once, and nothing for a transient state. A settled record rewritten
//! in place must give one newer native record version, an unchanged record
//! none, and the export must write the newest version.
#![cfg(all(feature = "sync", feature = "sqlite"))]

use protoledger::formats::{self, ExportOptions, ImportOptions};
use protoledger::store::Store;
use protoledger::sync::{self, FollowOptions, Follower};
use protoledger::{Event, Kind, Ledger};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const SID: &str = "ses_0000000000000LiveSyncTest01";
const MU: &str = "msg_0001aaaaaaaaUserMessage00";
const MA: &str = "msg_0002aaaaaaaaAssistantMsg0";
const P_USER: &str = "prt_0001aaaaaaaaUserText00000";
const P_TOOL1: &str = "prt_0002aaaaaaaaToolOne00000";
const P_TOOL2: &str = "prt_0003aaaaaaaaToolTwo00000";
const P_TEXT: &str = "prt_0004aaaaaaaaAssistText00";
const T: i64 = 1_792_000_000_000;

/// The follower's poll period, and how long a "nothing more" window lasts:
/// five poll periods.
const POLL: Duration = Duration::from_millis(200);
const QUIET: Duration = Duration::from_millis(1000);

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kindof {
    Storage,
    SqliteV1,
    SqliteV2,
}

impl Kindof {
    fn variant(self) -> &'static str {
        match self {
            Kindof::Storage => "v1-storage-json",
            Kindof::SqliteV1 => "v1-sqlite",
            Kindof::SqliteV2 => "v2-sqlite",
        }
    }
}

/// A live opencode store this test writes as opencode would: rows and files
/// rewritten in place.
struct Live {
    kind: Kindof,
    location: PathBuf,
}

fn without(v: &Value, keys: &[&str]) -> Value {
    let mut v = v.clone();
    for k in keys {
        v.as_object_mut().unwrap().remove(*k);
    }
    v
}

fn write_json(p: &Path, v: &Value) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, serde_json::to_string_pretty(v).unwrap()).unwrap();
}

fn session_v1() -> Value {
    json!({"id": SID, "version": "1.1.8", "projectID": "global", "directory": "/work", "title": "Live sync", "time": {"created": T, "updated": T}})
}

fn user_v1() -> (Value, Value) {
    let info = json!({"id": MU, "sessionID": SID, "role": "user", "time": {"created": T + 1}, "agent": "build", "model": {"providerID": "p", "modelID": "m"}});
    let part = json!({"id": P_USER, "sessionID": SID, "messageID": MU, "type": "text", "text": "Read two files."});
    (info, part)
}

/// Write the 1.x storage tree of a session holding one user prompt under `root`.
fn storage_base(root: &Path) {
    let st = root.join("storage");
    write_json(&st.join("session/global").join(format!("{SID}.json")), &session_v1());
    let (info, part) = user_v1();
    write_json(&st.join("message").join(SID).join(format!("{MU}.json")), &info);
    write_json(&st.join("part").join(MU).join(format!("{P_USER}.json")), &part);
}

impl Live {
    fn new(kind: Kindof, root: &Path) -> Live {
        let tree = root.join("oc");
        storage_base(&tree);
        let location = match kind {
            Kindof::Storage => tree,
            Kindof::SqliteV1 | Kindof::SqliteV2 => {
                // The same session, as the database that generation keeps (no carry:
                // what is read back is opencode's own records).
                let f = formats::by_name("opencode").unwrap();
                let imp = f.import(&tree, &ImportOptions { variant: Some("v1-storage-json".into()), restore_carried: false }).unwrap();
                let ex = f.export(&imp.set, &ExportOptions { variant: Some(kind.variant().into()), carry: false, ..Default::default() }).unwrap();
                let out = root.join("db");
                ex.output.write_to(&out, false).unwrap();
                let db = ex.output.files.keys().map(|k| out.join(k)).find(|p| p.extension().is_some_and(|x| x == "db")).expect("a database");
                let c = Connection::open(&db).unwrap();
                c.pragma_update(None, "journal_mode", "DELETE").unwrap();
                db
            }
        };
        Live { kind, location }
    }

    /// Write (insert or rewrite in place) a 1.x message info.
    fn put_message_v1(&self, m: &Value) {
        match self.kind {
            Kindof::Storage => write_json(&self.location.join("storage/message").join(SID).join(format!("{}.json", m["id"].as_str().unwrap())), m),
            Kindof::SqliteV1 => {
                let c = Connection::open(&self.location).unwrap();
                c.execute(
                    "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?3, ?4)
                     ON CONFLICT(id) DO UPDATE SET time_updated = excluded.time_updated, data = excluded.data",
                    rusqlite::params![m["id"].as_str().unwrap(), SID, m["time"]["created"].as_i64().unwrap(), without(m, &["id", "sessionID"]).to_string()],
                )
                .unwrap();
            }
            Kindof::SqliteV2 => unreachable!(),
        }
    }

    /// Write (insert or rewrite in place) a 1.x part.
    fn put_part_v1(&self, p: &Value) {
        let (id, mid) = (p["id"].as_str().unwrap(), p["messageID"].as_str().unwrap());
        match self.kind {
            Kindof::Storage => write_json(&self.location.join("storage/part").join(mid).join(format!("{id}.json")), p),
            Kindof::SqliteV1 => {
                let c = Connection::open(&self.location).unwrap();
                c.execute(
                    "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?4, ?5)
                     ON CONFLICT(id) DO UPDATE SET time_updated = excluded.time_updated, data = excluded.data",
                    rusqlite::params![id, mid, SID, T + 2, without(p, &["id", "sessionID", "messageID"]).to_string()],
                )
                .unwrap();
            }
            Kindof::SqliteV2 => unreachable!(),
        }
    }

    /// Write (insert or rewrite in place) a 2.x message row.
    fn put_message_v2(&self, m: &Value) {
        self.put_message_v2_on(&Connection::open(&self.location).unwrap(), m);
    }

    /// The same, through an open connection.
    fn put_message_v2_on(&self, c: &Connection, m: &Value) {
        let seq: i64 = c.query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM session_message", [], |r| r.get(0)).unwrap();
        c.execute(
            "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET time_updated = excluded.time_updated, data = excluded.data",
            rusqlite::params![
                m["id"].as_str().unwrap(),
                SID,
                m["type"].as_str().unwrap(),
                seq,
                m["time"]["created"].as_i64().unwrap(),
                without(m, &["id", "type"]).to_string()
            ],
        )
        .unwrap();
    }

    /// The record id the tool call `call_1` lives in.
    fn tool1_record(&self) -> &'static str {
        if self.kind == Kindof::SqliteV2 { MA } else { P_TOOL1 }
    }

    /// What a native record of this store holds for the logical value `v`
    /// written by [`Live`] (a row's `data` column; a file whole).
    fn stored(&self, v: &Value) -> Value {
        match self.kind {
            Kindof::Storage => v.clone(),
            Kindof::SqliteV1 if v.get("messageID").is_some() => without(v, &["id", "sessionID", "messageID"]),
            Kindof::SqliteV1 => without(v, &["id", "sessionID"]),
            Kindof::SqliteV2 => without(v, &["id", "type"]),
        }
    }

    /// The same, read from a native record.
    fn held(&self, rec: &Value) -> Value {
        match self.kind {
            Kindof::Storage => rec.clone(),
            _ => match &rec["data"] {
                Value::String(t) => serde_json::from_str(t).unwrap(),
                v => v.clone(),
            },
        }
    }
}

// ------------------------------------------------------------ the step's states

fn tool_v1(id: &str, call: &str, state: Value) -> Value {
    json!({"id": id, "sessionID": SID, "messageID": MA, "type": "tool", "callID": call, "tool": "read", "state": state})
}

fn assistant_v1(done: bool) -> Value {
    let mut m = json!({"id": MA, "sessionID": SID, "role": "assistant", "time": {"created": T + 2}, "parentID": MU, "modelID": "m", "providerID": "p", "mode": "build", "agent": "build", "path": {"cwd": "/work", "root": "/work"}, "cost": 0, "tokens": {"input": 0, "output": 0, "reasoning": 0, "cache": {"read": 0, "write": 0}}});
    if done {
        m["time"]["completed"] = json!(T + 9);
        m["tokens"] = json!({"input": 120, "output": 30, "reasoning": 0, "cache": {"read": 0, "write": 0}});
        m["finish"] = json!("stop");
    }
    m
}

/// The 1.x states, step by step: (message, parts).
fn states_v1(step: usize) -> (Value, Vec<Value>) {
    let st = |status: &str, n: i64| -> Value {
        match status {
            "pending" => json!({"status": "pending", "input": {}, "raw": ""}),
            "running" => json!({"status": "running", "input": {"filePath": format!("/work/{n}.txt")}, "time": {"start": T + 3}}),
            "completed" => {
                json!({"status": "completed", "input": {"filePath": format!("/work/{n}.txt")}, "output": "alpha", "title": "1.txt", "metadata": {}, "time": {"start": T + 3, "end": T + 4}})
            }
            "rewritten" => {
                json!({"status": "completed", "input": {"filePath": format!("/work/{n}.txt")}, "output": "alpha", "title": "1.txt", "metadata": {"truncated": false}, "time": {"start": T + 3, "end": T + 4}})
            }
            _ => json!({"status": "error", "input": {"filePath": format!("/work/{n}.txt")}, "error": "no such file", "time": {"start": T + 3, "end": T + 5}}),
        }
    };
    let text = |done: bool| -> Value {
        let mut p =
            json!({"id": P_TEXT, "sessionID": SID, "messageID": MA, "type": "text", "text": if done { "Done." } else { "Do" }, "time": {"start": T + 6}});
        if done {
            p["time"]["end"] = json!(T + 8);
        }
        p
    };
    let (s1, s2) = match step {
        2 => ("pending", "pending"),
        3 => ("running", "running"),
        4..=6 => ("completed", "error"),
        _ => ("rewritten", "error"),
    };
    let mut parts = vec![tool_v1(P_TOOL1, "call_1", st(s1, 1)), tool_v1(P_TOOL2, "call_2", st(s2, 2))];
    if step >= 4 {
        parts.push(text(step >= 5));
    }
    (assistant_v1(step >= 5), parts)
}

/// The 2.x assistant message, step by step.
fn state_v2(step: usize) -> Value {
    let tool = |call: &str, n: i64, status: &str| -> Value {
        let mut c = json!({"type": "tool", "id": call, "name": "read", "time": {"created": T + 3}});
        c["state"] = match status {
            "streaming" => json!({"status": "streaming", "input": "{\"path\":\"/wo"}),
            "running" => json!({"status": "running", "input": {"path": format!("/work/{n}.txt")}}),
            "completed" => {
                json!({"status": "completed", "input": {"path": format!("/work/{n}.txt")}, "content": [{"type": "text", "text": "alpha"}], "metadata": {}})
            }
            "rewritten" => {
                json!({"status": "completed", "input": {"path": format!("/work/{n}.txt")}, "content": [{"type": "text", "text": "alpha"}], "metadata": {"truncated": false}})
            }
            _ => json!({"status": "error", "input": {"path": format!("/work/{n}.txt")}, "error": {"type": "tool.execution", "message": "no such file"}}),
        };
        if status != "streaming" {
            c["time"]["ran"] = json!(T + 4);
        }
        if matches!(status, "completed" | "rewritten" | "error") {
            c["time"]["completed"] = json!(T + 5);
        }
        c
    };
    let (s1, s2) = match step {
        2 => ("streaming", "streaming"),
        3 => ("running", "running"),
        4..=6 => ("completed", "error"),
        _ => ("rewritten", "error"),
    };
    let mut content = vec![tool("call_1", 1, s1), tool("call_2", 2, s2)];
    if step >= 4 {
        content.push(json!({"type": "text", "text": if step >= 5 { "Done." } else { "Do" }}));
    }
    let mut m = json!({"id": MA, "type": "assistant", "time": {"created": T + 2}, "agent": "build", "model": {"id": "m", "providerID": "p", "variant": "default"}, "content": content});
    if step == 4 {
        // Running totals written while the response still streams: not yet the
        // step's usage (SPEC §3.1.1 places it when the response ended).
        m["tokens"] = json!({"input": 120, "output": 2, "reasoning": 0, "cache": {"read": 0, "write": 0}});
    }
    if step >= 5 {
        m["time"]["streamed"] = json!(T + 8);
        m["time"]["completed"] = json!(T + 9);
        m["finish"] = json!("stop");
        m["cost"] = json!(0.001);
        m["tokens"] = json!({"input": 120, "output": 30, "reasoning": 0, "cache": {"read": 0, "write": 0}});
    }
    m
}

impl Live {
    /// Write the step's state: every record of the step, rewritten in place.
    fn write_step(&self, step: usize) {
        if self.kind == Kindof::SqliteV2 {
            self.put_message_v2(&state_v2(step));
            return;
        }
        let (m, parts) = states_v1(step);
        self.put_message_v1(&m);
        for p in &parts {
            self.put_part_v1(p);
        }
    }
    /// opencode touching the session (its `time.updated` / `time_updated`), which
    /// it does on every message: a change to the location that is no fact.
    fn touch_session(&self, at: i64) {
        match self.kind {
            Kindof::Storage => {
                let mut v = session_v1();
                v["time"]["updated"] = json!(at);
                write_json(&self.location.join("storage/session/global").join(format!("{SID}.json")), &v);
            }
            Kindof::SqliteV1 => {
                let c = Connection::open(&self.location).unwrap();
                assert_eq!(c.execute("UPDATE session SET time_updated = ?1 WHERE id = ?2", rusqlite::params![at, SID]).unwrap(), 1);
            }
            Kindof::SqliteV2 => {
                let c = Connection::open(&self.location).unwrap();
                assert_eq!(c.execute("UPDATE session_v2 SET time_updated = ?1 WHERE id = ?2", rusqlite::params![at, SID]).unwrap(), 1);
            }
        }
    }
    /// The record holding `call_1` as the step wrote it, as this store holds it.
    fn tool1_at(&self, step: usize) -> Value {
        if self.kind == Kindof::SqliteV2 {
            return self.stored(&state_v2(step));
        }
        self.stored(&states_v1(step).1[0])
    }
}

// ------------------------------------------------------------ the follower

/// A fact the ledger states: (kind, call id or text).
fn fact(e: &Event) -> (Kind, String) {
    let what = match e.kind {
        Kind::ToolCall | Kind::ToolResult => e.str("call_id").unwrap_or("").to_string(),
        Kind::Text => e.str("text").unwrap_or("").to_string(),
        _ => String::new(),
    };
    (e.kind, what)
}

/// Step the follower until it has appended `n` events (or 5 s pass), then
/// one more [`QUIET`] window (five poll periods): returns everything appended.
fn follow(s: &mut Store, f: &mut Follower, n: usize) -> Vec<Event> {
    let mut got: Vec<Event> = Vec::new();
    let until = Instant::now() + Duration::from_secs(5);
    while got.len() < n && Instant::now() < until {
        for (_, r) in f.step(s, until).unwrap() {
            got.extend(r.appended.into_iter().map(|p| p.event));
        }
    }
    let quiet = Instant::now() + QUIET;
    while Instant::now() < quiet {
        for (_, r) in f.step(s, quiet).unwrap() {
            got.extend(r.appended.into_iter().map(|p| p.event));
        }
    }
    got
}

fn facts_of(evs: &[Event]) -> Vec<(Kind, String)> {
    let mut v: Vec<(Kind, String)> = evs.iter().map(fact).collect();
    v.sort();
    v
}

fn k(kind: Kind, what: &str) -> (Kind, String) {
    (kind, what.to_string())
}

/// The record a native of a ledger event carries, as the store holds it.
fn record(live: &Live, e: &Event) -> Value {
    let n = e.native.as_ref().expect("a native record");
    assert_eq!(n.part, 0, "{}: a newer version is carried as part 0", e.id);
    live.held(&n.record_json().unwrap_or_else(|| panic!("{} carries no record", e.id)))
}

fn drive(kind: Kindof) {
    let dir = tempfile::tempdir().unwrap();
    let live = Live::new(kind, dir.path());
    let mut s = Store::open(dir.path().join("store")).unwrap();
    let (b, _) = sync::attach(&mut s, "opencode", &live.location).unwrap();
    assert_eq!(b.agent, SID);
    let base = Ledger::read(&s.ledger_path(SID).unwrap()).unwrap().events.len();
    let mut f = Follower::new(&s, FollowOptions { debounce: Duration::from_millis(50), poll: POLL, watcher: false }).unwrap();

    // 2. The step begins; both tools' input is still arriving: nothing is a fact.
    let before = f.syncs();
    live.write_step(2);
    let got = follow(&mut s, &mut f, 0);
    assert!(f.syncs() > before, "{kind:?}: the follower never read the location in {QUIET:?}");
    assert_eq!(facts_of(&got), vec![], "{kind:?} step 2 (pending/streaming): {:?}", got.iter().map(|e| &e.id).collect::<Vec<_>>());

    // 3. Both tools run: their calls, carrying the record as it stands.
    live.write_step(3);
    let got = follow(&mut s, &mut f, 2);
    assert_eq!(facts_of(&got), vec![k(Kind::ToolCall, "call_1"), k(Kind::ToolCall, "call_2")], "{kind:?} step 3 (running)");
    let call1 = got.iter().find(|e| fact(e) == k(Kind::ToolCall, "call_1")).unwrap();
    assert_eq!(record(&live, call1), live.tool1_at(3), "{kind:?}: the call carries the running version");

    // 4. One completes, one fails, a text starts: the two results.
    live.write_step(4);
    let got = follow(&mut s, &mut f, 2);
    assert_eq!(facts_of(&got), vec![k(Kind::ToolResult, "call_1"), k(Kind::ToolResult, "call_2")], "{kind:?} step 4 (completed/error)");
    let res1 = got.iter().find(|e| fact(e) == k(Kind::ToolResult, "call_1")).unwrap();
    assert_eq!(live::rid(res1), live.tool1_record());
    assert_eq!(record(&live, res1), live.tool1_at(4), "{kind:?}: the result carries the completed version");
    let res2 = got.iter().find(|e| fact(e) == k(Kind::ToolResult, "call_2")).unwrap();
    assert_eq!(res2.body.get("is_error"), Some(&json!(true)), "{kind:?}");

    // 5. The text ended and the step finished: the text and the usage.
    live.write_step(5);
    let got = follow(&mut s, &mut f, 2);
    assert_eq!(facts_of(&got), vec![k(Kind::Text, "Done."), k(Kind::Usage, "")], "{kind:?} step 5 (ended)");

    // 6. Every record rewritten with what it already held, and the session
    // touched: no fact, no version.
    let before = f.syncs();
    live.write_step(6);
    live.touch_session(T + 100);
    let got = follow(&mut s, &mut f, 0);
    assert!(f.syncs() > before, "{kind:?}: the follower never read the rewrite");
    assert_eq!(got.iter().map(|e| e.id.clone()).collect::<Vec<_>>(), Vec::<String>::new(), "{kind:?} step 6 (unchanged)");

    // 7. A settled record rewritten in place: one newer version, on a native event.
    live.write_step(7);
    let got = follow(&mut s, &mut f, 1);
    assert_eq!(facts_of(&got), vec![k(Kind::Native, "")], "{kind:?} step 7 (rewritten in place)");
    assert_eq!(live::rid(&got[0]), live.tool1_record());
    assert_eq!(record(&live, &got[0]), live.tool1_at(7), "{kind:?}: the newer version");

    // 8. And again unchanged: nothing.
    let before = f.syncs();
    live.write_step(7);
    live.touch_session(T + 200);
    let got = follow(&mut s, &mut f, 0);
    assert!(f.syncs() > before, "{kind:?}: the follower never read the second rewrite");
    assert!(got.is_empty(), "{kind:?} step 8 (unchanged): {:?}", got.iter().map(|e| &e.id).collect::<Vec<_>>());

    // Each fact once, no id twice.
    let all = Ledger::read(&s.ledger_path(SID).unwrap()).unwrap().events;
    let mut ids: Vec<&str> = all.iter().map(|e| e.id.as_str()).collect();
    ids.sort();
    let n = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), n, "{kind:?}: an id is in the ledger twice");
    let gained = facts_of(&all[base..]);
    assert_eq!(
        gained,
        vec![
            k(Kind::Text, "Done."),
            k(Kind::ToolCall, "call_1"),
            k(Kind::ToolCall, "call_2"),
            k(Kind::ToolResult, "call_1"),
            k(Kind::ToolResult, "call_2"),
            k(Kind::Usage, ""),
            k(Kind::Native, ""),
        ]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>(),
        "{kind:?}: what the step added"
    );

    // The export writes the newest version of the rewritten record, once.
    let led = Ledger::read(&s.ledger_path(SID).unwrap()).unwrap();
    let mut set = protoledger::LedgerSet::new();
    set.insert(led);
    let fmt = formats::by_name("opencode").unwrap();
    let ex = fmt.export(&set, &ExportOptions { variant: Some(kind.variant().into()), carry: false, ..Default::default() }).unwrap();
    let out = dir.path().join("export");
    ex.output.write_to(&out, false).unwrap();
    let back = fmt.import(&out, &ImportOptions { variant: Some(kind.variant().into()), restore_carried: false }).unwrap();
    let l = back.set.get(SID).unwrap();
    let versions: Vec<Value> =
        l.events.iter().filter(|e| live::rid(e) == live.tool1_record() && e.native.as_ref().is_some_and(|n| n.part == 0)).map(|e| record(&live, e)).collect();
    assert_eq!(versions, vec![live.tool1_at(7)], "{kind:?}: the export holds the record once, at its newest version");
}

mod live {
    use protoledger::Event;
    pub fn rid(e: &Event) -> &str {
        e.native.as_ref().and_then(|n| n.id.as_deref()).unwrap_or("")
    }
}

#[test]
fn v1_storage_tree_settled_facts_once_newest_version_past_5_polls_of_200ms() {
    drive(Kindof::Storage);
}

#[test]
fn v1_sqlite_settled_facts_once_newest_version_past_5_polls_of_200ms() {
    drive(Kindof::SqliteV1);
}

#[test]
fn v2_sqlite_settled_facts_once_newest_version_past_5_polls_of_200ms() {
    drive(Kindof::SqliteV2);
}

/// An opencode session projected back into its own store, synced at once and
/// again after every file of it is rewritten with the same bytes: nothing is
/// appended, not even a native record version (the records did not change).
fn projected_then_synced_appends_nothing(fixture: &str) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path().join("store")).unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/opencode").join(fixture).join("native");
    let imp = s.import("opencode", &src).unwrap();
    let mut appended = 0;
    let mut bound = 0;
    let mut refused = Vec::new();
    for agent in &imp.created {
        let b = match sync::project(&mut s, agent, "opencode", &dir.path().join("out").join(agent)) {
            Ok(b) => b,
            Err(e) => {
                refused.push(format!("{agent}: {e:#}"));
                continue;
            }
        };
        bound += 1;
        let r = sync::sync(&mut s, &b.binding).unwrap();
        appended += r.appended.len();
        assert!(r.diverged.is_none(), "{fixture}: {r:?}");
        for e in walk(Path::new(&b.location)) {
            let bytes = std::fs::read(&e).unwrap();
            std::fs::write(&e, bytes).unwrap();
        }
        let r = sync::sync(&mut s, &b.binding).unwrap();
        appended += r.appended.len();
    }
    // Every agent the import created is projected and checked, not only those
    // whose projection happens to succeed.
    assert!(!imp.created.is_empty(), "{fixture}: the import created no agent");
    assert_eq!(bound, imp.created.len(), "{fixture}: not every created agent was projected: {refused:?}");
    assert_eq!(appended, 0, "{fixture}: a projection read back gained events");
}

fn walk(p: &Path) -> Vec<PathBuf> {
    if p.is_file() {
        return vec![p.to_path_buf()];
    }
    walkdir::WalkDir::new(p).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()).map(|e| e.into_path()).collect()
}

/// A real opencode store attached, then every file of it rewritten with the
/// same bytes and synced: nothing is appended (its settled records did not
/// change, so there is no newer version of any of them).
fn attached_then_rewritten_appends_nothing(fixture: &str) {
    let dir = tempfile::tempdir().unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/opencode").join(fixture).join("native");
    let copy = dir.path().join("native");
    for f in walk(&src) {
        let to = copy.join(f.strip_prefix(&src).unwrap());
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(&f, &to).unwrap();
    }
    let loc = if copy.join("opencode.db").exists() { copy.join("opencode.db") } else { copy.clone() };
    let mut s = Store::open(dir.path().join("store")).unwrap();
    let (b, first) = sync::attach(&mut s, "opencode", &loc).unwrap();
    assert!(!first.appended.is_empty(), "{fixture}: the attach imported nothing");
    let stamp = sync::stamp(&loc).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    for f in walk(&loc) {
        let bytes = std::fs::read(&f).unwrap();
        std::fs::write(&f, bytes).unwrap();
    }
    assert_ne!(sync::stamp(&loc).unwrap(), stamp, "{fixture}: the rewrite is a change sync sees");
    let r = sync::sync(&mut s, &b.binding).unwrap();
    assert!(r.diverged.is_none() && r.deferred.is_none(), "{fixture}: {r:?}");
    assert_eq!(r.appended.iter().map(|p| p.event.id.clone()).collect::<Vec<_>>(), Vec::<String>::new(), "{fixture}");
}

#[test]
fn attached_v1_storage_tree_rewritten_unchanged_appends_nothing() {
    attached_then_rewritten_appends_nothing("v1-storage-json-1.1.8-generated");
}

#[test]
fn attached_v1_sqlite_rewritten_unchanged_appends_nothing() {
    attached_then_rewritten_appends_nothing("v1-sqlite-1.2.21-generated");
}

#[test]
fn attached_v2_sqlite_rewritten_unchanged_appends_nothing() {
    attached_then_rewritten_appends_nothing("v2-sqlite-2.0.31-generated");
}

#[test]
fn projected_v1_sqlite_synced_appends_nothing() {
    projected_then_synced_appends_nothing("v1-sqlite-1.2.21-generated");
}

#[test]
fn projected_v2_sqlite_synced_appends_nothing() {
    projected_then_synced_appends_nothing("v2-sqlite-2.0.31-generated");
}

/// 2.x event ids are positions, and positions move: a tool that runs after the
/// response ended (`time.streamed`) gets its result placed right after its
/// call, before the text and usage the ledger already holds under the next
/// positions (`<message>#1`, `#2`). Each fact still reaches the ledger once:
/// the call, text and usage when the response ended, the result when the tool
/// completed, nothing more one [`QUIET`] window (five 200 ms polls) later.
#[test]
fn v2_moved_positions_append_each_fact_once_past_5_polls_of_200ms() {
    let dir = tempfile::tempdir().unwrap();
    let live = Live::new(Kindof::SqliteV2, dir.path());
    let mut s = Store::open(dir.path().join("store")).unwrap();
    sync::attach(&mut s, "opencode", &live.location).unwrap();
    let base = Ledger::read(&s.ledger_path(SID).unwrap()).unwrap().events.len();
    let mut f = Follower::new(&s, FollowOptions { debounce: Duration::from_millis(50), poll: POLL, watcher: false }).unwrap();
    let msg = |done: bool| -> Value {
        let state = if done {
            json!({"status": "completed", "input": {"path": "/work/1.txt"}, "content": [{"type": "text", "text": "alpha"}], "metadata": {}})
        } else {
            json!({"status": "running", "input": {"path": "/work/1.txt"}})
        };
        let mut tool = json!({"type": "tool", "id": "call_1", "name": "read", "state": state, "time": {"created": T + 3, "ran": T + 4}});
        if done {
            tool["time"]["completed"] = json!(T + 20);
        }
        let mut m = json!({"id": MA, "type": "assistant", "time": {"created": T + 2, "streamed": T + 10}, "agent": "build", "model": {"id": "m", "providerID": "p", "variant": "default"},
            "content": [tool, {"type": "text", "text": "Reading it."}], "cost": 0.001, "tokens": {"input": 120, "output": 30, "reasoning": 0, "cache": {"read": 0, "write": 0}}});
        if done {
            m["time"]["completed"] = json!(T + 21);
            m["finish"] = json!("tool-calls");
        }
        m
    };
    live.put_message_v2(&msg(false));
    let got = follow(&mut s, &mut f, 3);
    assert_eq!(facts_of(&got), vec![k(Kind::Text, "Reading it."), k(Kind::ToolCall, "call_1"), k(Kind::Usage, "")], "the response ended");
    live.put_message_v2(&msg(true));
    let got = follow(&mut s, &mut f, 1);
    assert_eq!(facts_of(&got), vec![k(Kind::ToolResult, "call_1")], "the tool completed");
    let all = Ledger::read(&s.ledger_path(SID).unwrap()).unwrap().events;
    assert_eq!(facts_of(&all[base..]), vec![k(Kind::Text, "Reading it."), k(Kind::ToolCall, "call_1"), k(Kind::ToolResult, "call_1"), k(Kind::Usage, "")]);
}

// ------------------------------------------------------------ write-ahead log

/// The bytes and modification time of a file, to show it did not change.
fn file_state(p: &Path) -> (Vec<u8>, std::time::SystemTime) {
    (std::fs::read(p).unwrap(), std::fs::metadata(p).unwrap().modified().unwrap())
}

/// A 2.x store in WAL mode (as opencode runs it), attached, with a follower;
/// then one completed assistant message written without a checkpoint, so it
/// sits in `opencode.db-wal` and `opencode.db` is not touched (checked, before
/// and after the sync): through `hold`, a connection that stays open as
/// opencode's does, or (`closing`) through another connection that closes
/// after the write while `hold` keeps the database open. Before it, files
/// beside the location that are not its WAL must make no read for a
/// [`QUIET`] second (five 200 ms polls; twenty 50 ms debounces). The follower
/// must append the message's facts before `deadline` (from the write), and
/// then read nothing more for [`QUIET`]: its own read changes nothing it
/// stamps or watches.
fn wal_write_syncs(opts: FollowOptions, deadline: Duration, closing: bool) {
    let dir = tempfile::tempdir().unwrap();
    let live = Live::new(Kindof::SqliteV2, dir.path());
    let hold = Connection::open(&live.location).unwrap();
    let mode: String = hold.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0)).unwrap();
    assert_eq!(mode, "wal");
    hold.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    // A read maps the WAL index, so this connection holds the database open:
    // no other connection's close is the last one, which would checkpoint.
    let _: i64 = hold.query_row("SELECT COUNT(*) FROM session_message", [], |r| r.get(0)).unwrap();
    let wal = PathBuf::from(format!("{}-wal", live.location.display()));
    let mut s = Store::open(dir.path().join("store")).unwrap();
    sync::attach(&mut s, "opencode", &live.location).unwrap();
    let mut f = Follower::new(&s, opts).unwrap();
    // Settle: the follower has read the location as it stands, and reads it no more.
    let settle = Instant::now() + Duration::from_millis(600);
    while Instant::now() < settle {
        assert!(f.step(&mut s, settle).unwrap().is_empty(), "nothing was written yet");
    }

    // Not-X first: files beside the location that are not its WAL (one whose
    // name starts with the WAL's) make no read for QUIET.
    let before = f.syncs();
    std::fs::write(PathBuf::from(format!("{}.bak", wal.display())), b"not the WAL").unwrap();
    std::fs::write(live.location.with_file_name("notes.txt"), b"not the location").unwrap();
    let quiet = Instant::now() + QUIET;
    while Instant::now() < quiet {
        assert!(f.step(&mut s, quiet).unwrap().is_empty());
    }
    assert_eq!(f.syncs(), before, "a file beside the location that is not its WAL made the follower read it");

    let db_before = file_state(&live.location);
    let wal_before = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
    std::thread::sleep(Duration::from_millis(20)); // a later mtime for anything written now
    let at = Instant::now();
    if closing {
        // Another connection writes and closes; `hold` keeps the database open,
        // so that close is not the last one and does not checkpoint.
        let c = Connection::open(&live.location).unwrap();
        c.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        live.put_message_v2_on(&c, &state_v2(5));
    } else {
        live.put_message_v2_on(&hold, &state_v2(5));
    }
    let wal_after = std::fs::metadata(&wal).unwrap().len();
    assert!(wal_after > wal_before, "the write went to the WAL ({wal_before} -> {wal_after} bytes)");
    assert!(file_state(&live.location) == db_before, "opencode.db itself is unchanged: the write sits in the WAL only");

    let mut got: Vec<Event> = Vec::new();
    let until = at + deadline;
    while got.is_empty() && Instant::now() < until {
        for (_, r) in f.step(&mut s, until).unwrap() {
            got.extend(r.appended.into_iter().map(|p| p.event));
        }
    }
    let took = at.elapsed();
    assert!(file_state(&live.location) == db_before, "no checkpoint happened: what the follower read came from the WAL");
    assert!(
        facts_of(&got).contains(&k(Kind::Text, "Done.")),
        "an uncheckpointed write was not synced within {deadline:?} (poll {:?}, watcher {}): {:?}",
        opts.poll,
        opts.watcher,
        facts_of(&got)
    );
    eprintln!("wal write synced in {took:?} (poll {:?}, watcher {})", opts.poll, opts.watcher);

    // And the follower's own read moved nothing it stamps or watches: no read for QUIET.
    let before = f.syncs();
    let quiet = Instant::now() + QUIET;
    while Instant::now() < quiet {
        assert!(f.step(&mut s, quiet).unwrap().is_empty());
    }
    assert_eq!(f.syncs(), before, "the follower re-read a location nobody wrote (its own read changed the stamp?)");
    drop(hold);
}

/// The poll alone (no watcher), the writer holding the database open as
/// opencode does: the write must be synced by the first poll after it (one
/// 200 ms period, plus 150 ms for the read itself).
#[test]
fn wal_uncheckpointed_write_synced_within_one_200ms_poll() {
    wal_write_syncs(FollowOptions { debounce: Duration::from_millis(50), poll: POLL, watcher: false }, POLL + Duration::from_millis(150), false);
}

/// The watcher alone: the poll is 60 s and never comes due in this test, so a
/// sync within 1 s can only come from the watcher reporting `opencode.db-wal`.
/// The writer's connection closes after the write: on macOS FSEvents reports a
/// file's content change when a writer closes it, so a writer that keeps the
/// WAL open (opencode while it runs) is seen by the poll, not the watcher
/// (measured: no event in 5 s; docs/LIVE-TESTS.md).
#[test]
fn wal_write_by_a_closing_connection_synced_by_watcher_within_1s_poll_60s() {
    wal_write_syncs(FollowOptions { debounce: Duration::from_millis(50), poll: Duration::from_secs(60), watcher: true }, Duration::from_secs(1), true);
}
