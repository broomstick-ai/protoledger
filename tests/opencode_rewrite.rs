//! Rows opencode rewrites in place in an attached `opencode.db` (SPEC §4.1).
//!
//! A todo row (in its session's ledger) or a store row (in the store ledger)
//! that opencode rewrites under the same key is a newer version of that row:
//! the next sync appends exactly one `native` event carrying the row as it
//! now stands, does not diverge and keeps following; a sync of an unchanged
//! database appends nothing; an export writes the newest version; a fresh
//! import of the database equals the synced ledgers' newest-version view. A
//! change then a deletion is a version then a tombstone; a deletion then a
//! row written again with other content is the new content. Messages and
//! parts keep their own rules: one rewritten so that events read earlier are
//! gone is still divergence, and one rewritten while it is still running
//! appends nothing.
#![cfg(all(feature = "sync", feature = "sqlite"))]

use protoledger::formats::{self, ExportOptions, ImportOptions};
use protoledger::store::{Published, Store};
use protoledger::sync;
use protoledger::{Kind, LedgerSet};
use rusqlite::Connection;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const V1: &str = "v1-sqlite-synthetic-1.2-todo";
const V2: &str = "v2-sqlite-2.0.31-generated";
/// The session of [`V1`] that holds three todo rows (positions 0, 1, 2;
/// position 2 is `pending`).
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
    /// Copy `fixture`'s database, run `prepare` on it (what opencode wrote
    /// before the attach), and attach it.
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

    fn ledgers(&self) -> LedgerSet {
        let mut set = LedgerSet::new();
        for a in self.store.agents().unwrap() {
            set.insert(self.store.reader(&a.id).unwrap().ledger().unwrap());
        }
        set
    }

    /// The store's ledgers exported to the database's own variant, then
    /// `query` (one text value) run on the written database.
    fn exported(&self, query: &str) -> Option<String> {
        let out = tempfile::tempdir().unwrap();
        let f = formats::by_name("opencode").unwrap();
        let ex = f.export(&self.ledgers(), &ExportOptions { variant: Some(self.variant.into()), ..Default::default() }).unwrap();
        ex.output.write_to(out.path(), false).unwrap();
        let db = ex.output.files.keys().map(|k| out.path().join(k)).find(|p| p.extension().is_some_and(|x| x == "db")).expect("a database");
        let c = Connection::open(&db).unwrap();
        let mut st = c.prepare(query).unwrap();
        let mut rows = st.query([]).unwrap();
        rows.next().unwrap().map(|r| r.get::<_, String>(0).unwrap())
    }

    fn reimport(&self) -> LedgerSet {
        formats::by_name("opencode").unwrap().import(&self.db, &ImportOptions::standard()).unwrap().set
    }

    fn store_ledger(&self) -> String {
        self.ledgers().iter().find(|l| l.id().starts_with("opencode:store")).expect("a store ledger").id().to_string()
    }
}

/// One ledger's newest-version view: per opencode record id, the text of its
/// newest version (a record whose newest version is a tombstone is left
/// out), and the ids of its neutral events. Computed here, from the events
/// alone, not by the code under test.
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

/// A sync of the database unchanged since the last one: nothing appended,
/// no divergence.
fn unchanged(a: &mut Attached, what: &str) {
    a.sql("SELECT 1");
    let r = a.sync();
    assert!(r.diverged.is_none() && r.appended.is_empty(), "{what}: an unchanged database appends nothing: {r:?}");
}

/// The one event `r` appended: a version (not a tombstone) of `rid` in
/// `agent`'s ledger, whose record contains `value`.
fn one_version<'a>(r: &'a sync::Synced, agent: &str, rid: &str, value: &str) -> &'a Published {
    assert!(r.diverged.is_none(), "a rewritten row is not divergence: {:?}", r.diverged);
    assert!(r.deferred.is_none(), "{r:?}");
    let got: Vec<(&str, Option<&str>)> = r.appended.iter().map(|p| (p.agent.as_str(), p.event.native.as_ref().and_then(|n| n.id.as_deref()))).collect();
    assert_eq!(got, vec![(agent, Some(rid))], "exactly one version event, of the rewritten row");
    let p = &r.appended[0];
    assert_eq!(p.event.kind, Kind::Native);
    assert!(!p.event.is_tombstone(), "a version, not a tombstone");
    assert_ne!(p.event.id, rid, "a later event, with an id of its own");
    let n = p.event.native.as_ref().unwrap();
    assert_eq!(n.part, 0);
    assert!(n.record_text().is_some_and(|t| t.contains(value)), "the version carries the row as it now stands ({value}): {:?}", n.record_text());
    p
}

/// Rewrite one row in place (`rewrite`) and check everything the module
/// docs promise: one version (its record contains `mark`), no divergence,
/// still followed, nothing on the next syncs, the export writes the new
/// value (`query` → `value`), a fresh import equals the newest-version view.
#[allow(clippy::too_many_arguments)]
fn rewritten_row_is_a_version(a: &mut Attached, rewrite: &str, agent: &str, rid: &str, query: &str, old: &str, value: &str, mark: &str) {
    assert!(a.ledgers().get(agent).unwrap().contains(rid), "{rid} is in {agent}'s ledger");
    assert_eq!(a.exported(query).as_deref(), Some(old), "before the rewrite the export writes the old value");
    unchanged(a, "before the rewrite");
    a.sql(rewrite);
    let r = a.sync();
    one_version(&r, agent, rid, mark);
    assert!(a.active(), "the binding keeps following");
    unchanged(a, "after the version");
    unchanged(a, "again");
    assert_eq!(a.exported(query).as_deref(), Some(value), "the export writes the newest version");
    assert_eq!(view(&a.ledgers()), view(&a.reimport()), "fresh import vs the synced newest-version view");
}

/// opencode replaces a session's todo list whole: every row deleted and
/// written again, with `change` applied to the copy.
fn replace_todos(change: &str) -> String {
    format!(
        "CREATE TEMP TABLE t AS SELECT * FROM todo WHERE session_id = '{TODO_SESSION}'; {change}; \
         DELETE FROM todo WHERE session_id = '{TODO_SESSION}'; INSERT INTO todo SELECT * FROM t; DROP TABLE t;"
    )
}

// (1) A todo row going pending → completed.
#[test]
fn v1_todo_pending_to_completed_is_one_version_in_the_session_ledger() {
    let mut a = Attached::new(V1, "");
    let rid = format!("@todo/{TODO_SESSION}/2");
    rewritten_row_is_a_version(
        &mut a,
        &replace_todos("UPDATE t SET status = 'completed', time_updated = 1777716099000 WHERE position = 2"),
        TODO_SESSION,
        &rid,
        "SELECT status FROM todo WHERE position = 2",
        "pending",
        "completed",
        "\"status\":\"completed\"",
    );
    // The two rows written again unchanged are still written, unchanged.
    assert_eq!(a.exported("SELECT group_concat(status) FROM todo WHERE position < 2").as_deref(), Some("completed,completed"));
}

// (2) Store rows rewritten in place.
#[test]
fn v2_kv_row_rewritten_is_one_version_in_the_store_ledger() {
    let mut a = Attached::new(V2, "INSERT INTO kv (key, value, time_created, time_updated) VALUES ('alpha', 'one', 1, 1), ('beta', 'two', 1, 1);");
    let st = a.store_ledger();
    rewritten_row_is_a_version(
        &mut a,
        "UPDATE kv SET value = 'uno', time_updated = 2 WHERE key = 'alpha';",
        &st,
        "@kv/alpha",
        "SELECT value FROM kv WHERE key = 'alpha'",
        "one",
        "uno",
        "uno",
    );
    assert_eq!(a.exported("SELECT value FROM kv WHERE key = 'beta'").as_deref(), Some("two"), "the row not rewritten");
}

#[test]
fn v2_worktree_row_rewritten_is_one_version_in_the_store_ledger() {
    let mut a = Attached::new(V2, "");
    let st = a.store_ledger();
    rewritten_row_is_a_version(
        &mut a,
        "UPDATE worktree SET strategy = 'zqstrategy';",
        &st,
        "@worktree/ce7d88ec139a55a32161a320d1a37563caf6e269//home/user/project-a",
        "SELECT coalesce(strategy, 'none') FROM worktree",
        "none",
        "zqstrategy",
        "zqstrategy",
    );
}

#[test]
fn v1_permission_row_rewritten_is_one_version_in_the_store_ledger() {
    let mut a = Attached::new(V1, "INSERT INTO permission (project_id, time_created, time_updated, data) SELECT id, 1, 1, '[]' FROM project;");
    let st = a.store_ledger();
    let pid: String = Connection::open(&a.db).unwrap().query_row("SELECT id FROM project", [], |r| r.get(0)).unwrap();
    rewritten_row_is_a_version(
        &mut a,
        "UPDATE permission SET data = '[\"zqallow\"]', time_updated = 2;",
        &st,
        &format!("@permission/{pid}"),
        "SELECT data FROM permission",
        "[]",
        "[\"zqallow\"]",
        "zqallow",
    );
}

#[test]
fn v1_project_row_rewritten_is_one_version_in_the_store_ledger() {
    let mut a = Attached::new(V1, "");
    let st = a.store_ledger();
    let pid: String = Connection::open(&a.db).unwrap().query_row("SELECT id FROM project", [], |r| r.get(0)).unwrap();
    let old: String = Connection::open(&a.db).unwrap().query_row("SELECT coalesce(name, 'none') FROM project", [], |r| r.get(0)).unwrap();
    rewritten_row_is_a_version(
        &mut a,
        "UPDATE project SET name = 'zqproject', time_updated = 1777716099000;",
        &st,
        &format!("@project/{pid}"),
        "SELECT coalesce(name, 'none') FROM project",
        &old,
        "zqproject",
        "zqproject",
    );
}

// (3) Opposing: rows that do not change get no version, however often synced,
// including rows opencode deletes and writes again unchanged.
#[test]
fn unchanged_rows_get_no_version_on_repeated_syncs() {
    let mut a = Attached::new(V1, "INSERT INTO permission (project_id, time_created, time_updated, data) SELECT id, 1, 1, '[]' FROM project;");
    for i in 0..3 {
        unchanged(&mut a, &format!("v1 sync {i}"));
    }
    // The todo list written again whole, every row as it was.
    a.sql(&replace_todos("SELECT 1"));
    let r = a.sync();
    assert!(r.diverged.is_none() && r.appended.is_empty(), "the todo list written again unchanged: {r:?}");
    let mut b = Attached::new(V2, "INSERT INTO kv (key, value, time_created, time_updated) VALUES ('alpha', 'one', 1, 1);");
    for i in 0..3 {
        unchanged(&mut b, &format!("v2 sync {i}"));
    }
    // The same value written again.
    b.sql("UPDATE kv SET value = 'one' WHERE key = 'alpha';");
    let r = b.sync();
    assert!(r.diverged.is_none() && r.appended.is_empty(), "a kv row written again unchanged: {r:?}");
}

// The rewrite is measured against the ledger's newest version, not the
// binding's cursor: a rewrite while the cursor was lost is still appended
// (a deletion in that window is not; see tests/opencode_delete.rs and the
// sync module docs).
#[test]
fn rewrite_while_the_cursor_was_lost_is_still_a_version() {
    let mut a = Attached::new(V1, "");
    a.store.set_cursor(&a.binding, "").unwrap();
    a.sql(&replace_todos("UPDATE t SET status = 'completed' WHERE position = 2"));
    let r = a.sync();
    one_version(&r, TODO_SESSION, &format!("@todo/{TODO_SESSION}/2"), "completed");
    unchanged(&mut a, "after");
}

// (4) Change then delete: a version, then a tombstone; written again with
// other content: that content.
#[allow(clippy::too_many_arguments)]
fn changed_deleted_recreated(fixture: &str, prepare: &str, agent: Option<&str>, rid: &str, steps: [&str; 3], query: &str, values: [&str; 2], marks: [&str; 2]) {
    let mut a = Attached::new(fixture, prepare);
    let agent = agent.map(str::to_string).unwrap_or_else(|| a.store_ledger());
    a.sql(steps[0]);
    let r = a.sync();
    one_version(&r, &agent, rid, marks[0]);
    assert_eq!(a.exported(query).as_deref(), Some(values[0]));
    a.sql(steps[1]);
    let r = a.sync();
    assert!(r.diverged.is_none() && a.active(), "{r:?}");
    let t: Vec<(&str, bool, Option<&str>)> =
        r.appended.iter().map(|p| (p.agent.as_str(), p.event.is_tombstone(), p.event.native.as_ref().and_then(|n| n.id.as_deref()))).collect();
    assert_eq!(t, vec![(agent.as_str(), true, Some(rid))], "then exactly one tombstone");
    assert_eq!(a.exported(query), None, "the deleted row is not written");
    assert_eq!(view(&a.ledgers()), view(&a.reimport()));
    unchanged(&mut a, "after the tombstone");
    a.sql(steps[2]);
    let r = a.sync();
    one_version(&r, &agent, rid, marks[1]);
    assert_eq!(a.exported(query).as_deref(), Some(values[1]), "the row written again, with its new content");
    assert_eq!(view(&a.ledgers()), view(&a.reimport()));
    unchanged(&mut a, "after it came back");
    // Versions of the record in the ledger, in order: the first, the change,
    // the tombstone, the new content.
    let l = a.ledgers();
    let kinds: Vec<&str> = l
        .get(&agent)
        .unwrap()
        .events
        .iter()
        .filter(|e| e.native.as_ref().is_some_and(|n| n.id.as_deref() == Some(rid)))
        .map(|e| if e.is_tombstone() { "tombstone" } else { "version" })
        .collect();
    assert_eq!(kinds, ["version", "version", "tombstone", "version"]);
}

#[test]
fn v1_todo_changed_then_deleted_then_written_again() {
    let rid = format!("@todo/{TODO_SESSION}/2");
    changed_deleted_recreated(
        V1,
        "",
        Some(TODO_SESSION),
        &rid,
        [
            &replace_todos("UPDATE t SET status = 'completed' WHERE position = 2"),
            &replace_todos("DELETE FROM t WHERE position = 2"),
            &format!(
                "INSERT INTO todo (session_id, content, status, priority, position, time_created, time_updated) VALUES ('{TODO_SESSION}', 'zqagain', 'pending', 'high', 2, 3, 3);"
            ),
        ],
        "SELECT status || ':' || content FROM todo WHERE position = 2",
        ["completed:List notes/", "pending:zqagain"],
        ["\"status\":\"completed\"", "zqagain"],
    );
}

#[test]
fn v2_kv_changed_then_deleted_then_written_again() {
    changed_deleted_recreated(
        V2,
        "INSERT INTO kv (key, value, time_created, time_updated) VALUES ('alpha', 'one', 1, 1);",
        None,
        "@kv/alpha",
        [
            "UPDATE kv SET value = 'uno' WHERE key = 'alpha';",
            "DELETE FROM kv WHERE key = 'alpha';",
            "INSERT INTO kv (key, value, time_created, time_updated) VALUES ('alpha', 'zqagain', 3, 3);",
        ],
        "SELECT value FROM kv WHERE key = 'alpha'",
        ["uno", "zqagain"],
        ["uno", "zqagain"],
    );
}

// (5) Messages and parts keep their own rules; the rows' version path does
// not mask them.

/// A 2.x step (a text, then a tool) rewritten in place with its content cut
/// to the text:
/// events read earlier are gone, which is divergence, as before; nothing is
/// appended (no version of the message), and the binding stops.
#[test]
fn v2_message_rewritten_shorter_in_place_still_diverges() {
    let mut a = Attached::new(V2, "");
    unchanged(&mut a, "before");
    a.sql("UPDATE session_message SET data = json_set(data, '$.content', json_array(json_extract(data, '$.content[0]'))) WHERE id = 'msg_14735e3950011ZKrknERlhcNxX';");
    let r = a.sync();
    assert!(r.diverged.as_deref().is_some_and(|d| d.contains("msg_14735e3950011ZKrknERlhcNxX")), "a message rewritten shorter is divergence: {r:?}");
    assert!(r.appended.is_empty(), "nothing appended: {:?}", r.appended);
    assert!(!a.active(), "the binding stops");
}

/// A 1.x tool part rewritten while it is still running (its progress
/// metadata): a transient state, so nothing is appended, as before; once it
/// completes, its result is appended.
#[test]
fn v1_running_tool_part_rewritten_in_place_appends_nothing() {
    let part = "prt_de821c320001fX7FsmQVJapx4A";
    let mut a =
        Attached::new(V1, &format!("UPDATE part SET data = json_set(json_remove(data, '$.state.output'), '$.state.status', 'running') WHERE id = '{part}';"));
    unchanged(&mut a, "before");
    a.sql(&format!("UPDATE part SET data = json_set(data, '$.state.metadata', json('{{\"zqprogress\":1}}')) WHERE id = '{part}';"));
    let r = a.sync();
    assert!(r.diverged.is_none() && r.appended.is_empty(), "a running part rewritten in place is not a version: {r:?}");
    // Control: the same part completing is appended (the rewrite was read).
    a.sql(&format!("UPDATE part SET data = json_set(data, '$.state.status', 'completed', '$.state.output', '') WHERE id = '{part}';"));
    let r = a.sync();
    assert!(r.diverged.is_none(), "{r:?}");
    assert!(r.appended.iter().any(|p| p.event.kind == Kind::ToolResult), "the completed tool's result: {:?}", r.appended);
}
