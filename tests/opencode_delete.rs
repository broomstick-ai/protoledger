//! Rows opencode deletes from an attached `opencode.db` (SPEC §4.1, §9.3).
//!
//! A row opencode deletes on its own (a worktree, a kv or permission row in
//! the store ledger, a todo row in a session's ledger) is a newer version of
//! that row: the next sync appends one tombstone for it (a `native` event
//! naming the row, body `{"deleted": true}`), does not diverge and keeps
//! following; an export leaves the row out; a fresh import of the database
//! equals the synced ledgers' newest-version view. A row deleted and written
//! again under the same key comes back with its new value. A message gone
//! from a session is still divergence, with or without a deleted row beside it.
#![cfg(all(feature = "sync", feature = "sqlite"))]

use protoledger::formats::{self, ExportOptions, ImportOptions};
use protoledger::store::Store;
use protoledger::sync;
use protoledger::{Event, Kind, Ledger, LedgerSet};
use rusqlite::Connection;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const V1: &str = "v1-sqlite-synthetic-1.2-todo";
const V2: &str = "v2-sqlite-2.0.31-generated";
/// The session of [`V1`] that holds three todo rows (positions 0, 1, 2).
const TODO_SESSION: &str = "ses_217de8affffegpbQFxp4Z2A2Ng";

/// A copy of a fixture's `opencode.db`, attached to a fresh store.
struct Attached {
    _dir: tempfile::TempDir,
    db: PathBuf,
    store: Store,
    binding: String,
    variant: &'static str,
}

impl Attached {
    /// Copy `fixture`'s database, run `prepare` on it (rows opencode would
    /// have written before the attach), and attach it.
    fn new(fixture: &str, prepare: &str) -> Attached {
        let dir = tempfile::tempdir().unwrap();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/opencode").join(fixture).join("native/opencode.db");
        let db = dir.path().join("native/opencode.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        std::fs::copy(&src, &db).unwrap();
        let c = Connection::open(&db).unwrap();
        c.pragma_update(None, "journal_mode", "DELETE").unwrap();
        c.execute_batch(prepare).unwrap();
        drop(c);
        let mut store = Store::open(dir.path().join("store")).unwrap();
        let (b, first) = sync::attach(&mut store, "opencode", &db).unwrap();
        assert!(!first.appended.is_empty(), "{fixture}: the attach imported nothing");
        let variant = if fixture == V1 { "v1-sqlite" } else { "v2-sqlite" };
        Attached { _dir: dir, db, store, binding: b.binding, variant }
    }

    /// Change the database as opencode would.
    fn sql(&self, sql: &str) {
        // A new modification time, so the change is one sync sees.
        std::thread::sleep(std::time::Duration::from_millis(20));
        Connection::open(&self.db).unwrap().execute_batch(sql).unwrap();
    }

    fn sync(&mut self) -> sync::Synced {
        sync::sync(&mut self.store, &self.binding).unwrap()
    }

    fn active(&self) -> bool {
        self.store.binding(&self.binding).unwrap().unwrap().active
    }

    /// Every ledger the store holds.
    fn ledgers(&self) -> LedgerSet {
        let mut set = LedgerSet::new();
        for a in self.store.agents().unwrap() {
            set.insert(self.store.reader(&a.id).unwrap().ledger().unwrap());
        }
        set
    }

    /// The store's ledgers exported to the database's own variant: the
    /// path of the written database.
    fn export(&self, out: &Path) -> PathBuf {
        let f = formats::by_name("opencode").unwrap();
        let ex = f.export(&self.ledgers(), &ExportOptions { variant: Some(self.variant.into()), ..Default::default() }).unwrap();
        ex.output.write_to(out, false).unwrap();
        ex.output.files.keys().map(|k| out.join(k)).find(|p| p.extension().is_some_and(|x| x == "db")).expect("a database")
    }

    /// A fresh import of the database as it stands now.
    fn reimport(&self) -> LedgerSet {
        formats::by_name("opencode").unwrap().import(&self.db, &ImportOptions::standard()).unwrap().set
    }
}

/// One ledger's newest-version view: per opencode record id, the text of its
/// newest version (a record whose newest version is a tombstone is left
/// out), and the ids of its neutral events (protoledger's own `app` facts
/// left out).
type View = (BTreeMap<String, String>, Vec<String>);

fn view(set: &LedgerSet) -> BTreeMap<String, View> {
    let mut out = BTreeMap::new();
    for l in set.iter() {
        let mut records: BTreeMap<String, Option<String>> = BTreeMap::new();
        let mut neutral = Vec::new();
        for e in &l.events {
            match e.native.as_ref().filter(|n| n.format == "opencode" && n.part == 0) {
                Some(n) if e.is_tombstone() => {
                    records.insert(n.id.clone().unwrap(), None);
                }
                Some(n) if n.record.is_some() => {
                    records.insert(n.id.clone().unwrap(), n.record_text().map(str::to_string));
                }
                _ => {}
            }
            if !matches!(e.kind, Kind::Native | Kind::App) {
                neutral.push(e.id.clone());
            }
        }
        let records = records.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))).collect();
        out.insert(l.id().to_string(), (records, neutral));
    }
    out
}

/// The tombstones among `evs`, as (agent, record id).
fn tombstones(evs: &[protoledger::store::Published]) -> Vec<(String, String)> {
    evs.iter().filter(|p| p.event.is_tombstone()).map(|p| (p.agent.clone(), p.event.native.as_ref().unwrap().id.clone().unwrap())).collect()
}

fn rows(db: &Path, sql: &str) -> i64 {
    Connection::open(db).unwrap().query_row(sql, [], |r| r.get(0)).unwrap()
}

/// The store ledger of an attached database.
fn store_ledger(a: &Attached) -> String {
    a.ledgers().iter().find(|l| l.id().starts_with("opencode:store")).expect("a store ledger").id().to_string()
}

/// Delete the rows `delete` names (records `gone`, in ledger `agent`), and
/// check everything the module docs promise. `count_gone` counts the
/// deleted rows in an export.
fn deleted_row_is_a_version(a: &mut Attached, delete: &str, agent: &str, gone: &[&str], count_gone: &str) {
    // Nothing changed: nothing is appended, no tombstone for a row that is still there.
    a.sql("SELECT 1");
    let r = a.sync();
    assert!(r.diverged.is_none() && r.appended.is_empty(), "an unchanged database: {r:?}");
    a.sql(delete);
    let r = a.sync();
    assert!(r.diverged.is_none(), "a deleted row is not divergence: {:?}", r.diverged);
    assert!(r.deferred.is_none(), "{r:?}");
    assert!(a.active(), "the binding keeps following");
    let want: Vec<(String, String)> = gone.iter().map(|g| (agent.to_string(), g.to_string())).collect();
    assert_eq!(tombstones(&r.appended), want, "exactly one tombstone per deleted row");
    assert_eq!(r.appended.len(), gone.len(), "nothing but the tombstones: {:?}", r.appended);
    for p in &r.appended {
        assert_eq!(p.event.body.get("deleted"), Some(&Value::Bool(true)));
        assert_eq!(p.event.kind, Kind::Native);
    }
    // A second sync of the same database appends nothing more.
    a.sql("SELECT 1");
    let r = a.sync();
    assert!(r.diverged.is_none() && r.appended.is_empty(), "the deletion is appended once: {r:?}");
    // An export leaves the deleted rows out.
    let out = tempfile::tempdir().unwrap();
    let db = a.export(out.path());
    assert_eq!(rows(&db, count_gone), 0, "an export writes no deleted row");
    // A fresh import of the database equals the synced ledgers' newest versions.
    assert_eq!(view(&a.ledgers()), view(&a.reimport()), "fresh import vs the synced newest-version view");
}

/// Then it keeps following: a row added afterwards is synced as an append.
fn keeps_following(a: &mut Attached, insert: &str) {
    a.sql(insert);
    let r = a.sync();
    assert!(r.diverged.is_none() && a.active(), "{r:?}");
    assert_eq!(r.appended.len(), 1, "the new row: {:?}", r.appended);
    assert!(!r.appended[0].event.is_tombstone());
    assert_eq!(view(&a.ledgers()), view(&a.reimport()));
}

const V2_KV: &str = "INSERT INTO kv (key, value, time_created, time_updated) VALUES ('alpha', 'one', 1, 1), ('beta', 'two', 1, 1);";

#[test]
fn v2_deleted_worktree_row_is_a_tombstone_not_divergence() {
    let mut a = Attached::new(V2, "");
    let st = store_ledger(&a);
    let wt = "@worktree/ce7d88ec139a55a32161a320d1a37563caf6e269//home/user/project-a";
    assert!(a.ledgers().get(&st).unwrap().contains(wt), "the worktree row is in the store ledger");
    deleted_row_is_a_version(&mut a, "DELETE FROM worktree;", &st, &[wt], "SELECT COUNT(*) FROM worktree");
    keeps_following(&mut a, "INSERT INTO kv (key, value, time_created, time_updated) VALUES ('later', 'x', 2, 2);");
}

#[test]
fn v2_deleted_kv_row_is_a_tombstone_and_the_other_row_stays() {
    let mut a = Attached::new(V2, V2_KV);
    let st = store_ledger(&a);
    deleted_row_is_a_version(&mut a, "DELETE FROM kv WHERE key = 'alpha';", &st, &["@kv/alpha"], "SELECT COUNT(*) FROM kv WHERE key = 'alpha'");
    let out = tempfile::tempdir().unwrap();
    assert_eq!(rows(&a.export(out.path()), "SELECT COUNT(*) FROM kv WHERE key = 'beta'"), 1, "the row not deleted is still written");
    keeps_following(&mut a, "INSERT INTO kv (key, value, time_created, time_updated) VALUES ('gamma', 'three', 2, 2);");
}

#[test]
fn v1_deleted_permission_row_is_a_tombstone_not_divergence() {
    // 1.x has no kv table; a permission row is the session-less row it has.
    let mut a = Attached::new(V1, "INSERT INTO permission (project_id, time_created, time_updated, data) SELECT id, 1, 1, '[]' FROM project;");
    let st = store_ledger(&a);
    let pid: String = Connection::open(&a.db).unwrap().query_row("SELECT id FROM project", [], |r| r.get(0)).unwrap();
    let rid = format!("@permission/{pid}");
    deleted_row_is_a_version(&mut a, "DELETE FROM permission;", &st, &[&rid], "SELECT COUNT(*) FROM permission");
    keeps_following(
        &mut a,
        "INSERT INTO control_account (email, url, access_token, refresh_token, token_expiry, active, time_created, time_updated) VALUES ('a@example.com', 'https://example.com', 'x', 'y', NULL, 0, 2, 2);",
    );
}

#[test]
fn v1_deleted_todo_row_is_a_tombstone_in_the_session_ledger() {
    let mut a = Attached::new(V1, "");
    let rid = format!("@todo/{TODO_SESSION}/2");
    assert!(a.ledgers().get(TODO_SESSION).unwrap().contains(&rid), "the todo row is in its session's ledger");
    deleted_row_is_a_version(&mut a, "DELETE FROM todo WHERE position = 2;", TODO_SESSION, &[&rid], "SELECT COUNT(*) FROM todo WHERE position = 2");
    let out = tempfile::tempdir().unwrap();
    assert_eq!(rows(&a.export(out.path()), "SELECT COUNT(*) FROM todo"), 2, "the two todo rows not deleted are still written");
    keeps_following(
        &mut a,
        &format!(
            "INSERT INTO todo (session_id, content, status, priority, position, time_created, time_updated) VALUES ('{TODO_SESSION}', 'Later', 'pending', 'low', 3, 2, 2);"
        ),
    );
}

#[test]
fn v1_two_todo_rows_deleted_give_two_tombstones() {
    let mut a = Attached::new(V1, "");
    let gone = [format!("@todo/{TODO_SESSION}/0"), format!("@todo/{TODO_SESSION}/1")];
    let gone: Vec<&str> = gone.iter().map(String::as_str).collect();
    deleted_row_is_a_version(&mut a, "DELETE FROM todo WHERE position < 2;", TODO_SESSION, &gone, "SELECT COUNT(*) FROM todo WHERE position < 2");
}

/// A row deleted, then written again under the same key with a new value:
/// one more version, carrying the new value; the export writes it.
fn recreated_comes_back(fixture: &str, prepare: &str, delete: &str, recreate: &str, agent: &str, rid: &str, value: &str) {
    let mut a = Attached::new(fixture, prepare);
    let agent = if agent.is_empty() { store_ledger(&a) } else { agent.to_string() };
    a.sql(delete);
    let r = a.sync();
    assert_eq!(tombstones(&r.appended), vec![(agent.clone(), rid.to_string())]);
    a.sql(recreate);
    let r = a.sync();
    assert!(r.diverged.is_none() && a.active(), "{r:?}");
    assert_eq!(r.appended.len(), 1, "one newer version: {:?}", r.appended);
    let e: &Event = &r.appended[0].event;
    assert!(!e.is_tombstone() && e.kind == Kind::Native);
    let n = e.native.as_ref().unwrap();
    assert_eq!(n.id.as_deref(), Some(rid));
    assert!(n.record_text().unwrap().contains(value), "the version carries the row as written again: {:?}", n.record_text());
    assert_eq!(view(&a.ledgers()), view(&a.reimport()), "the re-created row is the newest version");
    let out = tempfile::tempdir().unwrap();
    let db = a.export(out.path());
    let (table, key) = match rid.trim_start_matches('@').split_once('/').unwrap() {
        ("kv", k) => ("kv", format!("key = '{k}'")),
        ("todo", k) => ("todo", format!("position = {}", k.rsplit('/').next().unwrap())),
        other => panic!("{other:?}"),
    };
    let held: String = Connection::open(&db)
        .unwrap()
        .query_row(&format!("SELECT {} FROM {table} WHERE {key}", if table == "kv" { "value" } else { "content" }), [], |r| r.get(0))
        .unwrap();
    assert_eq!(held, value, "the export writes the newest version");
    // Synced again, unchanged: nothing more.
    a.sql("SELECT 1");
    let r = a.sync();
    assert!(r.diverged.is_none() && r.appended.is_empty(), "{r:?}");
}

#[test]
fn v2_kv_row_deleted_then_recreated_comes_back_newest_version_wins() {
    recreated_comes_back(
        V2,
        V2_KV,
        "DELETE FROM kv WHERE key = 'alpha';",
        "INSERT INTO kv (key, value, time_created, time_updated) VALUES ('alpha', 'again', 3, 3);",
        "",
        "@kv/alpha",
        "again",
    );
}

#[test]
fn v1_todo_row_deleted_then_recreated_comes_back_newest_version_wins() {
    recreated_comes_back(
        V1,
        "",
        "DELETE FROM todo WHERE position = 2;",
        &format!(
            "INSERT INTO todo (session_id, content, status, priority, position, time_created, time_updated) VALUES ('{TODO_SESSION}', 'Rewritten', 'pending', 'high', 2, 3, 3);"
        ),
        TODO_SESSION,
        &format!("@todo/{TODO_SESSION}/2"),
        "Rewritten",
    );
}

/// A message gone from a session (the conversation truncated) is divergence,
/// and a deleted row beside it does not hide that: nothing is appended, not
/// even the tombstone, and the binding stops.
fn message_gone_still_diverges(fixture: &str, delete: &str) {
    let mut a = Attached::new(fixture, "");
    a.sql(delete);
    let r = a.sync();
    assert!(r.diverged.is_some(), "{fixture}: a message gone is divergence: {r:?}");
    assert!(r.appended.is_empty(), "{fixture}: nothing appended: {:?}", r.appended);
    assert!(!a.active(), "{fixture}: the binding stops");
}

#[test]
fn v1_truncated_session_still_diverges() {
    message_gone_still_diverges(
        V1,
        "DELETE FROM part WHERE message_id = 'msg_de8221528001m81X0Ji2nFt8HZ'; DELETE FROM message WHERE id = 'msg_de8221528001m81X0Ji2nFt8HZ';",
    );
}

#[test]
fn v1_truncated_session_with_a_deleted_todo_row_still_diverges() {
    message_gone_still_diverges(
        V1,
        "DELETE FROM todo WHERE position = 2; DELETE FROM part WHERE message_id = 'msg_de8221528001m81X0Ji2nFt8HZ'; DELETE FROM message WHERE id = 'msg_de8221528001m81X0Ji2nFt8HZ';",
    );
}

#[test]
fn v2_truncated_session_with_a_deleted_worktree_row_still_diverges() {
    message_gone_still_diverges(V2, "DELETE FROM worktree; DELETE FROM session_message WHERE id = 'msg_1472ba57e002y220WcuiqcvFiq';");
}

#[test]
fn v2_truncated_session_still_diverges() {
    message_gone_still_diverges(V2, "DELETE FROM session_message WHERE id = 'msg_1472ba57e002y220WcuiqcvFiq';");
}

/// A ledger holding a tombstone exported by itself (no sync): the deleted
/// row is left out, a row with no tombstone is written.
#[test]
fn export_leaves_out_a_row_whose_newest_version_is_a_tombstone() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/opencode").join(V1).join("native/opencode.db");
    let f = formats::by_name("opencode").unwrap();
    let mut set = f.import(&src, &ImportOptions::standard()).unwrap().set;
    let rid = format!("@todo/{TODO_SESSION}/1");
    let l: &mut Ledger = set.ledgers.get_mut(TODO_SESSION).unwrap();
    let n = l.events.iter().find(|e| e.id == rid).unwrap().native.clone().unwrap();
    l.events.push(Event::tombstone(format!("{rid}#1"), &n));
    let out = tempfile::tempdir().unwrap();
    let ex = f.export(&set, &ExportOptions { variant: Some("v1-sqlite".into()), ..Default::default() }).unwrap();
    ex.output.write_to(out.path(), false).unwrap();
    let db = out.path().join("opencode.db");
    assert_eq!(rows(&db, "SELECT COUNT(*) FROM todo WHERE position = 1"), 0, "the tombstoned row");
    assert_eq!(rows(&db, "SELECT COUNT(*) FROM todo"), 2, "the other two rows");
}
