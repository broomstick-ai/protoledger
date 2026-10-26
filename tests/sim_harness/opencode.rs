//! A simulated opencode 2.x turn on a projected ledger.
//!
//! The shapes come from two committed fixtures written by real opencode
//! runs: `fixtures/opencode/_live-revert-2.0.32` (opencode 2.0.32: the
//! `user` row, the `assistant` row's fields, the `idle` row that ends a turn,
//! `seq` advancing with gaps) and `fixtures/opencode/v2-sqlite-2.0.31-generated`
//! (opencode 2.0.31: an assistant row holding a `read` tool call with its
//! result, `snapshot`, `finish: tool-calls`/`stop`, `tokens`). The states a
//! tool passes through before `completed` (`streaming` with its input still a
//! string, then `running` with `time.ran`) are those of
//! `tests/opencode_sync.rs`, which follow opencode's 2.x schema.
//!
//! The simulator opens the projected `opencode.db` the way opencode runs it:
//! one connection, held open for the whole turn, in WAL mode, with automatic
//! checkpoints off, so every write of the turn sits in `opencode.db-wal` and
//! `opencode.db` itself does not change (checked, before and after). On that
//! connection it writes the prompt row, then rewrites the assistant row in
//! place as the response streams (tool `streaming` → `running` → `completed`,
//! a text growing, the message completing with its usage), then the `idle`
//! row, touching `session_v2.time_updated` as it goes, each write after a
//! delay.

use crate::common::{self, Mark, Paths};
use crate::modes::Mode;
use protoledger::Kind;
use protoledger::formats::{self, ImportOptions};
use protoledger::store::Store;
use protoledger::sync;
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const SOURCE: &str = "opencode/v2-sqlite-2.0.31-generated/native/opencode.db";
/// The fixture's `s1_simple` session: one plain reply.
const SID: &str = "ses_eb8d577daffemT7plHSFKatVSE";
const M_USER: &str = "msg_1472ff0a1001SimUserPrompt01";
const M_ASSIST: &str = "msg_1472ff0b2001SimAssistant001";
const M_IDLE: &str = "msg_1472ff9c3001SimIdleRecord01";
const CALL: &str = "6f1c2a9e-0b7d-4c55-9a3e-5d8f1e2b7c40";
/// The time the turn starts, in opencode's milliseconds (after the fixture session's last row).
const T: i64 = 1_792_195_500_000;

fn to_json(v: ValueRef) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::from(i),
        ValueRef::Real(f) => Value::from(f),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::String(format!("<{} byte blob>", b.len())),
    }
}

/// The session's `session_message` rows: id → (columns, `data` text), read
/// through a connection of its own (which sees what the WAL holds).
fn rows(db: &Path) -> BTreeMap<String, (serde_json::Map<String, Value>, String)> {
    let c = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut st = c.prepare("SELECT * FROM session_message WHERE session_id = ?1 ORDER BY seq").unwrap();
    let cols: Vec<String> = st.column_names().iter().map(|c| c.to_string()).collect();
    let mut q = st.query([SID]).unwrap();
    let mut out = BTreeMap::new();
    while let Some(r) = q.next().unwrap() {
        let mut o = serde_json::Map::new();
        for (i, c) in cols.iter().enumerate() {
            o.insert(c.clone(), to_json(r.get_ref(i).unwrap()));
        }
        let data = o.get("data").and_then(|d| d.as_str()).unwrap_or("").to_string();
        out.insert(o["id"].as_str().unwrap().to_string(), (o, data));
    }
    out
}

fn import_ids(loc: &Path) -> Vec<String> {
    let imp = formats::by_name("opencode").unwrap().import(loc, &ImportOptions::standard()).unwrap();
    imp.set.get(SID).unwrap_or_else(|| panic!("{} holds no {SID}", loc.display())).events.iter().map(|e| e.id.clone()).collect()
}

/// The assistant row's `data` as the response stands at `step`.
fn assistant(step: usize) -> Value {
    let model = json!({"id": "big-pickle", "providerID": "opencode", "variant": "default"});
    let input = json!({"path": "/home/user/project-a/input.txt", "offset": "1.0", "limit": "50.0"});
    let mut tool = json!({"type": "tool", "id": CALL, "name": "read", "executed": false, "time": {"created": T + 1200}});
    tool["state"] = match step {
        0 => json!({"status": "streaming", "input": "{\"path\":\"/home/user/proj"}),
        1 => json!({"status": "running", "input": input}),
        _ => json!({"status": "completed", "input": input,
                    "content": [{"type": "text", "text": "Read file /home/user/project-a/input.txt, lines 1-3\n1: alpha\n2: beta\n3: gamma"}],
                    "metadata": {"truncated": false}}),
    };
    if step >= 1 {
        tool["time"]["ran"] = json!(T + 1210);
    }
    if step >= 2 {
        tool["time"]["completed"] = json!(T + 1220);
    }
    let mut content = vec![tool];
    if step >= 3 {
        content.push(json!({"type": "text", "text": if step >= 4 { "It holds alpha, beta and gamma." } else { "It holds" }}));
    }
    let mut m = json!({"time": {"created": T + 20}, "agent": "build", "model": model, "content": content});
    if step >= 4 {
        m["time"]["streamed"] = json!(T + 2400);
        m["time"]["completed"] = json!(T + 2410);
        m["snapshot"] = json!({"start": "5941e240b600fcebf8dd43c705d4e726de3428dc", "end": "5941e240b600fcebf8dd43c705d4e726de3428dc", "files": []});
        m["finish"] = json!("stop");
        m["rawFinish"] = json!("stop");
        m["cost"] = json!(0);
        m["tokens"] = json!({"input": 3610, "output": 31, "reasoning": 12, "cache": {"read": 1792, "write": 0}});
    }
    m
}

/// Insert or rewrite one `session_message` row, and touch the session, on `c`.
fn put(c: &Connection, id: &str, kind: &str, seq: i64, created: i64, updated: i64, data: &Value) {
    c.execute(
        "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(id) DO UPDATE SET time_updated = excluded.time_updated, data = excluded.data",
        rusqlite::params![id, SID, kind, seq, created, updated, data.to_string()],
    )
    .unwrap();
    assert_eq!(c.execute("UPDATE session_v2 SET time_updated = ?1 WHERE id = ?2", rusqlite::params![updated, SID]).unwrap(), 1);
}

fn file_state(p: &Path) -> (Vec<u8>, std::time::SystemTime) {
    (std::fs::read(p).unwrap(), std::fs::metadata(p).unwrap().modified().unwrap())
}

/// Import the fixture's database into a store, project the session into a
/// fresh temporary data directory (the `opencode.db` that `v2-sqlite`
/// exports), follow it with the default options, let the simulator write a
/// turn into the WAL, and check the live-sync properties on the ledger.
#[test]
fn opencode_simulated_turn_in_wal_once_each_exact_within_2s_no_divergence_for_a_1s_poll_after() {
    opencode_turn(Mode::Plain);
}

/// The test above in a store in `m` (retrained, in that mode, once the
/// session is projected and before the turn).
fn opencode_turn(m: Mode) {
    let mut paths = Paths::new();
    let src = common::fixture(SOURCE);
    paths.fixture(&src);
    let store_dir = paths.root.join("store");
    m.prepare(&store_dir);
    let mut s = Store::open(&store_dir).unwrap();
    s.import("opencode", &src).unwrap();
    let dir = paths.dir("opencode-data/opencode");
    let b = sync::project(&mut s, SID, "opencode", &dir).unwrap();
    m.retrain(&mut s);
    common::live_sealing(m, &mut s);
    let loc = PathBuf::from(&b.location);
    assert_eq!(loc, dir.join("opencode.db"), "the projection is the data directory's opencode.db");
    assert_eq!(b.variant.as_deref(), Some("v2-sqlite"));
    paths.wrote(&loc);
    let wal = PathBuf::from(format!("{}-wal", loc.display()));
    paths.wrote(&wal);
    paths.wrote(&PathBuf::from(format!("{}-shm", loc.display())));

    // opencode's connection: WAL, no automatic checkpoint, held open (a read
    // maps the WAL index, so no other connection's close checkpoints).
    let hold = Connection::open(&loc).unwrap();
    let mode: String = hold.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0)).unwrap();
    assert_eq!(mode, "wal");
    hold.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    let seq0: i64 = hold.query_row("SELECT COALESCE(MAX(seq), 0) FROM session_message WHERE session_id = ?1", [SID], |r| r.get(0)).unwrap();
    let pre_rows = rows(&loc);
    let pre = s.reader(SID).unwrap().ledger().unwrap().events;
    let pre_ids = import_ids(&loc);

    let mut follower = common::follower(&s);
    // Let the follower read the location as it stands (the journal-mode switch
    // rewrote the database header) before the turn starts.
    if let Some(f) = follower.as_mut() {
        let settle = Instant::now() + Duration::from_millis(1300);
        while Instant::now() < settle {
            assert!(f.step(&mut s, settle).unwrap().iter().all(|(_, r)| r.appended.is_empty()), "nothing was written yet");
        }
    }
    let db_before = file_state(&loc);
    let wal_before = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);

    // Shared with the simulator's thread, and held here past the checks below:
    // opencode's TUI or server stays up after a turn, and nothing checkpoints.
    let hold = std::sync::Arc::new(std::sync::Mutex::new(hold));
    let sim_hold = hold.clone();
    let sim = move |tx: std::sync::mpsc::Sender<Mark>| {
        let c = &*sim_hold.lock().unwrap();
        let pace = |ms: u64| std::thread::sleep(Duration::from_millis(ms));
        pace(50);
        put(c, M_USER, "user", seq0 + 3, T, T + 1, &json!({"time": {"created": T}, "text": "\"Read input.txt and say what it holds.\"", "files": []}));
        pace(120);
        let steps: [(u64, i64); 5] = [(300, 1100), (250, 1215), (400, 1225), (300, 1900), (350, 2410)];
        for (i, (delay, at)) in steps.iter().enumerate() {
            put(c, M_ASSIST, "assistant", seq0 + 6, T + 20, T + at, &assistant(i));
            pace(*delay);
        }
        put(c, M_IDLE, "idle", seq0 + 11, T + 2412, T + 2412, &json!({"time": {"created": T + 2412}, "outcome": "succeeded"}));
        let _ = tx.send(Mark::End(Instant::now()));
    };
    let want_loc = loc.clone();
    let held: std::collections::HashSet<String> = pre.iter().map(|e| e.id.clone()).collect();
    let want_fn = move || import_ids(&want_loc).into_iter().filter(|id| !held.contains(id)).collect::<Vec<_>>();
    // PL_SIM_FORCE=late-divergence: delete the turn's prompt row once every
    // event is in, so the next sync finds events it read gone.
    let cut = hold.clone();
    let diverge = move || {
        cut.lock().unwrap().execute("DELETE FROM session_message WHERE id = ?1", [M_USER]).unwrap();
    };
    let w = common::follow_turn(&mut s, follower.as_mut(), &b.binding, sim, want_fn.clone(), diverge);
    let all = s.reader(SID).unwrap().ledger().unwrap().events;
    // First, before anything reads the location again: no sync up to a poll
    // period past the last event found it diverged.
    common::no_divergence(&all, &w, "opencode");

    // The writes sat in the WAL: the database file is as it was, and the WAL grew.
    let wal_after = std::fs::metadata(&wal).unwrap().len();
    assert!(wal_after > wal_before, "the turn went to the WAL ({wal_before} -> {wal_after} bytes)");
    assert!(file_state(&loc) == db_before, "opencode.db itself is unchanged: the turn sits in the WAL, uncheckpointed");

    // What the turn wrote, read back independently of sync.
    let now_rows = rows(&loc);
    let touched: Vec<&String> = now_rows.iter().filter(|(id, r)| pre_rows.get(*id) != Some(r)).map(|(id, _)| id).collect();
    assert_eq!(touched, [M_USER, M_ASSIST, M_IDLE], "the rows the turn wrote");
    // The importer reads the projected part as before: every id it gave, in
    // the same order (session-level events derived from `session_v2` sort
    // after the messages, so new rows come before them).
    let now_ids = import_ids(&loc);
    let mut it = now_ids.iter();
    assert!(pre_ids.iter().all(|id| it.any(|x| x == id)), "the importer reads the projected part as before: {pre_ids:?} in {now_ids:?}");
    let want = want_fn();
    let got = &all[pre.len()..];
    eprintln!("sim: opencode: {} new events from {} rows; {} syncs changed the store", want.len(), touched.len(), w.changed.len());
    for e in got {
        eprintln!("sim:   {} {:?} native {:?}", e.id, e.kind, e.native.as_ref().and_then(|n| n.id.as_deref()));
    }

    // 1. Once each; the only other events allowed are a settled record's
    // newer version (SPEC §4.1), which a fresh import has no event for.
    common::once_each(got, &want, "opencode", |e| e.kind == Kind::Native && e.native.as_ref().is_some_and(|n| n.id.as_deref() == Some(M_ASSIST)));
    // 2. Within 2 s of the turn's end.
    common::within_bound(&want, &w, "opencode");
    // 3. Reads while the response streamed: no divergence. (opencode's store
    // is a database: every sync of it reads it whole, through the in-place
    // hook, SPEC §4.1; there is no growing file to read on from.)
    common::read_mid_turn(&w, "opencode");
    let tool = |status: &str| {
        got.iter().filter(|e| e.str("call_id") == Some(CALL) && e.kind == if status == "call" { Kind::ToolCall } else { Kind::ToolResult }).count()
    };
    assert_eq!((tool("call"), tool("result")), (1, 1), "the tool call once (when it ran) and its result once (when it completed)");
    assert_eq!(got.iter().filter(|e| e.kind == Kind::Usage).count(), 1, "the step's usage once");

    // 4. Native records: the newest version of each row the turn wrote is the row.
    for id in &touched {
        let (cols, data) = &now_rows[*id];
        let newest = all
            .iter()
            .rfind(|e| e.native.as_ref().is_some_and(|n| n.part == 0 && n.id.as_deref() == Some(id.as_str()) && n.record.is_some()))
            .unwrap_or_else(|| panic!("row {id}: no native record of it in the ledger"));
        let n = newest.native.as_ref().unwrap();
        let rec = n.record_json().unwrap();
        let held_data = n.raw.clone().unwrap_or_else(|| serde_json::to_string(&rec["data"]).unwrap());
        assert_eq!(&held_data, data, "row {id}: the newest native record's data is not the row's bytes (event {})", newest.id);
        for (k, v) in cols.iter().filter(|(k, _)| k.as_str() != "data") {
            assert_eq!(rec.get(k), Some(v), "row {id}: column {k} (event {})", newest.id);
        }
    }
    eprintln!("sim: opencode: {} rows compared byte for byte: equal", touched.len());

    paths.store(&s);
    paths.assert_isolated("opencode");
    drop(hold);
    drop(s);
    m.check(&store_dir);
}

modes_tests!(opencode_turn);
