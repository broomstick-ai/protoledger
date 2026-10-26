//! The store (SPEC §9): append → query, reindex equivalence, crash catch-up,
//! subscribe, one writer per ledger, import. The reindex tests run in a
//! plain store (their own names) and in each compressed mode
//! (`<test>::no_dictionary`, `::passed_dictionary`, `::retrained`).
#![cfg(feature = "store")]

#[macro_use]
#[path = "modes/mod.rs"]
mod modes;

use protoledger::store::{AgentInfo, BindingRow, BlockRow, EventRow, Published, Since, Store};
use protoledger::{Agent, Appender, Event, Kind, Link, Parent, Relation, Source};
use std::path::{Path, PathBuf};

fn agent(id: &str) -> Agent {
    Agent { id: id.into(), created_at: None, source: None, parent: None, links: vec![], meta: Default::default() }
}

fn link(rel: &str, to: Option<&str>) -> Link {
    Link { rel: rel.into(), agent: to.map(Into::into), event: None, extra: Default::default() }
}

fn text(id: &str, t: &str) -> Event {
    Event::new(id, Kind::Text).with("role", "user").with("text", t)
}

const RELS: [&str; 5] = ["spawned-by", "forked-from", "continued-from", "continued-in", "member-of"];

/// Every answer the index gives, for every agent the store holds.
#[derive(Debug, PartialEq)]
struct Snapshot {
    agents: Vec<AgentInfo>,
    per_agent: Vec<PerAgent>,
    bindings: Vec<BindingRow>,
    /// Blocks per dictionary per ledger (SPEC §10.8).
    dict_usage: Vec<(Option<String>, String, u64)>,
}

#[derive(Debug, PartialEq)]
struct PerAgent {
    agent: Option<AgentInfo>,
    children: Vec<AgentInfo>,
    linked_from: Vec<(String, Vec<AgentInfo>)>,
    events: Vec<EventRow>,
    page: Vec<EventRow>,
    at: Vec<Option<Event>>,
    blocks: Vec<BlockRow>,
}

fn snapshot(s: &Store) -> Snapshot {
    let agents = s.agents().unwrap();
    let per_agent = agents
        .iter()
        .map(|a| PerAgent {
            agent: s.agent(&a.id).unwrap(),
            children: s.children(&a.id).unwrap(),
            linked_from: RELS.iter().map(|r| (r.to_string(), s.linked_from(&a.id, r).unwrap())).collect(),
            events: s.events(&a.id, 0, usize::MAX).unwrap(),
            page: s.events(&a.id, 2, 2).unwrap(),
            at: (0..=a.last_seq + 1).map(|q| s.event_at(&a.id, q).unwrap()).collect(),
            blocks: s.blocks(&a.id).unwrap(),
        })
        .collect();
    Snapshot { agents, per_agent, bindings: s.bindings().unwrap(), dict_usage: s.dictionary_usage().unwrap() }
}

/// A store with a parent, a subagent, a spawned peer with two links, a fork,
/// titles, turns, groups, timestamps and an unknown kind.
fn populated(root: &Path) -> Store {
    populated_in(root, modes::Mode::Plain)
}

/// [`populated`], in a store in `m` (retrained, in that mode, once seeded).
fn populated_in(root: &Path, m: modes::Mode) -> Store {
    m.prepare(root);
    let mut s = Store::open(root).unwrap();
    let mut a = agent("a");
    a.created_at = Some("2026-10-16T10:00:00Z".into());
    a.source = Some(Source {
        format: "claude-code".into(),
        variant: Some("jsonl-sidecar".into()),
        version: Some("2.1.234".into()),
        native_id: Some("sess-1".into()),
        extra: serde_json::from_str(r#"{"slug":"-home-me"}"#).unwrap(),
    });
    s.create_agent(a).unwrap();
    s.append("a", text("a1", "hi").ts(Some("2026-10-16T10:00:01Z")).turn(Some("t1"))).unwrap();
    s.append("a", Event::new("a2", Kind::Title).with("text", "first title")).unwrap();
    s.append("a", Event::new("a3", Kind::ToolCall).with("call_id", "c1").with("name", "Task").group(Some("m1")).turn(Some("t1"))).unwrap();
    let mut sub = agent("a.sub");
    sub.parent = Some(Parent { agent: "a".into(), event: Some("a3".into()), relation: Relation::Subagent });
    s.create_agent(sub).unwrap();
    s.append("a.sub", text("s1", "do it")).unwrap();
    s.append("a", Event::new("a4", Kind::Title).with("text", "second title")).unwrap();
    let mut odd = Event::new("a5", Kind::Native).after(Some("a1"));
    odd.unknown_kind = Some("hologram".into());
    s.append("a", odd).unwrap();
    let mut p = agent("p1");
    p.links = vec![link("spawned-by", Some("a")), link("member-of", None)];
    s.create_agent(p).unwrap();
    s.append("p1", text("p1-1", "peer")).unwrap();
    let mut f = agent("f");
    f.links = vec![link("forked-from", Some("a"))];
    s.create_agent(f).unwrap();
    // Bindings (SPEC §9.2): one bound then unbound, one still bound.
    use protoledger::store::bindings::{Bound, Mode, bound_event, unbound_event};
    let bound = |id: &str, mode| Bound {
        binding: id.into(),
        format: "claude-code".into(),
        variant: Some("jsonl-sidecar".into()),
        location: format!("/nowhere/{id}.jsonl"),
        native_id: Some(format!("native-{id}")),
        mode,
    };
    s.append("f", bound_event(&bound("b-gone", Mode::Projection))).unwrap();
    s.append("f", unbound_event("b-gone")).unwrap();
    s.append("p1", bound_event(&bound("b-live", Mode::Attachment))).unwrap();
    m.retrain(&mut s);
    s
}

/// The seeded bindings, as `populated` leaves them.
fn seeded_bindings() -> Vec<BindingRow> {
    let row = |b: &str, agent: &str, mode: &str, active, ended: Option<&str>| BindingRow {
        binding: b.into(),
        agent: agent.into(),
        format: "claude-code".into(),
        variant: Some("jsonl-sidecar".into()),
        location: format!("/nowhere/{b}.jsonl"),
        native_id: Some(format!("native-{b}")),
        mode: mode.into(),
        active,
        ended: ended.map(Into::into),
    };
    vec![row("b-gone", "f", "projection", false, Some("unbound")), row("b-live", "p1", "attachment", true, None)]
}

fn index_files(root: &Path) -> Vec<PathBuf> {
    ["index.db", "index.db-wal", "index.db-shm"].iter().map(|n| root.join(n)).collect()
}

/// Write into the index file directly, behind the store's back.
fn poke_index(root: &Path, sql: &str) {
    let db = rusqlite::Connection::open(root.join("index.db")).unwrap();
    db.execute_batch(sql).unwrap();
}

#[test]
fn append_then_query() {
    let dir = tempfile::tempdir().unwrap();
    let s = populated(dir.path());
    assert!(dir.path().join("ledgers/a.jsonl").is_file());
    assert!(dir.path().join("index.db").is_file());

    let ids: Vec<_> = s.agents().unwrap().into_iter().map(|a| a.id).collect();
    assert_eq!(ids, ["a", "a.sub", "f", "p1"]);
    let a = s.agent("a").unwrap().unwrap();
    assert_eq!(a.last_seq, 5);
    assert_eq!(a.title.as_deref(), Some("second title"));
    assert_eq!(a.source.as_ref().unwrap().native_id.as_deref(), Some("sess-1"));
    assert_eq!(a.byte_len, std::fs::metadata(dir.path().join("ledgers/a.jsonl")).unwrap().len());
    assert_eq!(s.agent("nobody").unwrap(), None);

    let kids: Vec<_> = s.children("a").unwrap().into_iter().map(|a| a.id).collect();
    assert_eq!(kids, ["a.sub"]);
    assert!(s.children("p1").unwrap().is_empty());
    let spawned: Vec<_> = s.linked_from("a", "spawned-by").unwrap().into_iter().map(|a| a.id).collect();
    assert_eq!(spawned, ["p1"]);
    let forked: Vec<_> = s.linked_from("a", "forked-from").unwrap().into_iter().map(|a| a.id).collect();
    assert_eq!(forked, ["f"]);
    assert!(s.linked_from("p1", "spawned-by").unwrap().is_empty());

    let rows = s.events("a", 0, usize::MAX).unwrap();
    assert_eq!(
        rows.iter().map(|r| (r.seq, r.id.as_str(), r.kind.as_str())).collect::<Vec<_>>(),
        [(1, "a1", "text"), (2, "a2", "title"), (3, "a3", "tool_call"), (4, "a4", "title"), (5, "a5", "hologram")]
    );
    assert_eq!(rows[0].ts.as_deref(), Some("2026-10-16T10:00:01Z"));
    assert_eq!(rows[2].group.as_deref(), Some("m1"));
    assert_eq!(rows[2].turn.as_deref(), Some("t1"));
    for w in rows.windows(2) {
        assert_eq!(w[0].offset + w[0].len, w[1].offset, "rows tile the file");
    }
    let page: Vec<_> = s.events("a", 2, 2).unwrap().into_iter().map(|r| r.seq).collect();
    assert_eq!(page, [2, 3]);
    assert!(s.events("a", 6, 10).unwrap().is_empty());

    let e = s.event_at("a", 3).unwrap().unwrap();
    assert_eq!((e.id.as_str(), e.seq, e.str("name")), ("a3", 3, Some("Task")));
    assert_eq!(s.event_at("a", 5).unwrap().unwrap().unknown_kind.as_deref(), Some("hologram"));
    assert_eq!(s.event_at("a", 6).unwrap(), None);
    assert_eq!(s.event_at("a", 0).unwrap(), None);
}

#[test]
fn create_agent_refuses_a_duplicate_and_a_missing_parent() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    s.create_agent(agent("a")).unwrap();
    assert!(s.create_agent(agent("a")).is_err());
    let mut orphan = agent("o");
    orphan.parent = Some(Parent { agent: "missing".into(), event: None, relation: Relation::Subagent });
    assert!(s.create_agent(orphan).is_err());
    assert!(s.append("missing", text("x", "x")).is_err());
    let ids: Vec<_> = s.agents().unwrap().into_iter().map(|a| a.id).collect();
    assert_eq!(ids, ["a"]);
}

/// SPEC §9.1: delete index.db, reindex, and every answer is what it was.
/// The two operands are taken from two different index files: the old one is
/// first shown to be what the queries read (a sentinel written into it shows
/// up, and makes the comparison come out unequal), then deleted.
#[test]
fn reindex_after_deleting_index_db_reproduces_every_answer() {
    reindex_after_deleting_index_db(modes::Mode::Plain);
}

fn reindex_after_deleting_index_db(m: modes::Mode) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let before = snapshot(&populated_in(root, m));
    m.check(root);
    // In a compressed mode, the index names a dictionary for every sealed block.
    let rows: usize = before.per_agent.iter().map(|p| p.blocks.len()).sum();
    assert_eq!(rows > 0, m.compressed(), "{m:?}: {rows} block rows");
    assert!(before.per_agent.iter().flat_map(|p| &p.blocks).all(|b| b.dict == m.dictionary()), "{m:?}");
    assert_eq!(before.agents.len(), 4);
    assert!(before.per_agent.iter().map(|p| p.events.len()).sum::<usize>() >= 7);
    assert_eq!(before.bindings, seeded_bindings(), "the bindings operand is not empty");

    // Sentinel: the queries answer from index.db, not from the ledgers.
    poke_index(root, "UPDATE agents SET title = 'SENTINEL' WHERE id = 'a'; DELETE FROM events WHERE agent = 'a' AND seq = 3;");
    {
        let s = Store::open(root).unwrap();
        assert_eq!(s.agent("a").unwrap().unwrap().title.as_deref(), Some("SENTINEL"));
        assert_ne!(snapshot(&s), before, "the comparison can come out unequal");
    }

    for f in index_files(root) {
        if f.exists() {
            std::fs::remove_file(&f).unwrap();
        }
    }
    assert!(index_files(root).iter().all(|f| !f.exists()), "every index file is gone");

    let mut s = Store::open(root).unwrap();
    assert!(root.join("index.db").is_file(), "a new index file");
    let r = s.reindex().unwrap();
    assert_eq!((r.agents, r.events), (4, 10));
    let after = snapshot(&s);
    assert_eq!(after.agents.iter().find(|a| a.id == "a").unwrap().title.as_deref(), Some("second title"));
    assert_eq!(after, before);
}

modes_tests!(reindex_after_deleting_index_db, reindex_in_place);

/// Reindex in place throws away everything the ledgers do not say.
#[test]
fn reindex_in_place_discards_what_the_ledgers_do_not_say() {
    reindex_in_place(modes::Mode::Plain);
}

fn reindex_in_place(m: modes::Mode) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let before = snapshot(&populated_in(root, m));
    m.check(root);
    assert_eq!(before.bindings, seeded_bindings());
    poke_index(
        root,
        "UPDATE bindings SET active = 1, ended = NULL WHERE binding = 'b-gone';
         INSERT INTO bindings (binding, agent, format, location, mode, active) VALUES ('b-ghost', 'f', 'x', '/x', 'attachment', 1);
         UPDATE agents SET title = 'SENTINEL', last_seq = 99 WHERE id = 'a';
         UPDATE events SET grp = 'wrong' WHERE agent = 'a';
         UPDATE blocks SET dict = 'wrong';
         INSERT INTO agents (id, path, size, mtime_ns, last_seq, byte_len, links) VALUES ('ghost', 'ledgers/ghost.jsonl', 0, 0, 0, 0, '[]');",
    );
    let mut s = Store::open(root).unwrap();
    assert_ne!(snapshot(&s), before);
    s.reindex().unwrap();
    assert_eq!(snapshot(&s), before);
}

/// Appends made by a raw Appender while no store was open are indexed on the
/// next open, from the last indexed offset; a new ledger file is picked up;
/// a torn tail is not indexed until it is completed.
#[test]
fn reopen_catches_up_appends_made_behind_the_stores_back() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    {
        let mut s = Store::open(root).unwrap();
        s.create_agent(agent("a")).unwrap();
        s.create_agent(agent("quiet")).unwrap();
        s.append("a", text("e1", "one")).unwrap();
        s.append("a", text("e2", "two")).unwrap();
        s.append("quiet", text("q1", "q")).unwrap();
    }
    let quiet_before = Store::open(root).unwrap().agent("quiet").unwrap().unwrap();
    {
        let mut ap = Appender::open(&root.join("ledgers/a.jsonl"), None).unwrap();
        ap.append(text("e3", "three")).unwrap();
        ap.append(Event::new("e4", Kind::Title).with("text", "behind")).unwrap();
        let mut b = Appender::open(&root.join("ledgers/b.jsonl"), Some(agent("b"))).unwrap();
        b.append(text("b1", "new file")).unwrap();
    }
    use std::io::Write;
    std::fs::OpenOptions::new().append(true).open(root.join("ledgers/b.jsonl")).unwrap().write_all(b"{\"id\":\"b2\"").unwrap();

    let s = Store::open(root).unwrap();
    let a = s.agent("a").unwrap().unwrap();
    assert_eq!(a.last_seq, 4, "the index caught up");
    assert_eq!(a.title.as_deref(), Some("behind"));
    let ids: Vec<_> = s.events("a", 0, 100).unwrap().into_iter().map(|r| r.id).collect();
    assert_eq!(ids, ["e1", "e2", "e3", "e4"]);
    assert_eq!(s.event_at("a", 3).unwrap().unwrap().str("text"), Some("three"));
    assert_eq!(s.agent("quiet").unwrap().unwrap(), quiet_before, "an untouched ledger is left as it was");
    let b = s.agent("b").unwrap().unwrap();
    assert_eq!(b.last_seq, 1, "the torn line is not indexed");
    assert!(b.size > b.byte_len);
}

/// The same, while the store is open but has not yet taken the ledger's lock:
/// its first append indexes what it missed before its own event.
#[test]
fn first_append_catches_up_what_a_raw_writer_added() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    {
        let mut s = Store::open(root).unwrap();
        s.create_agent(agent("a")).unwrap();
        s.append("a", text("e1", "one")).unwrap();
    }
    let mut s = Store::open(root).unwrap();
    let rx = s.subscribe(Since::All).unwrap();
    assert_eq!(rx.try_recv().unwrap().event.id, "e1");
    {
        let mut ap = Appender::open(&root.join("ledgers/a.jsonl"), None).unwrap();
        ap.append(text("e2", "raw")).unwrap();
    }
    assert_eq!(s.append("a", text("e3", "store")).unwrap().seq, 3);
    let ids: Vec<_> = s.events("a", 0, 100).unwrap().into_iter().map(|r| r.id).collect();
    assert_eq!(ids, ["e1", "e2", "e3"]);
    let got: Vec<_> = rx.try_iter().map(|p| (p.seq, p.event.id)).collect();
    assert_eq!(got, [(2, "e2".to_string()), (3, "e3".to_string())]);
}

fn drain(rx: &std::sync::mpsc::Receiver<Published>) -> Vec<(String, u64, String)> {
    // Publishing is synchronous: everything sent is in the channel before
    // append() returns, so draining with try_recv waits on no clock.
    rx.try_iter().map(|p| (p.agent, p.seq, p.event.id)).collect()
}

fn ev(agent: &str, seq: u64, id: &str) -> (String, u64, String) {
    (agent.to_string(), seq, id.to_string())
}

#[test]
fn subscribe_receives_appends_in_order_and_resumes_from_seq() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    s.create_agent(agent("a")).unwrap();
    s.create_agent(agent("b")).unwrap();
    let live = s.subscribe(Since::All).unwrap();
    assert!(drain(&live).is_empty());

    let mut offsets = Vec::new();
    for (who, id) in [("a", "a1"), ("b", "b1"), ("a", "a2"), ("b", "b2"), ("a", "a3")] {
        offsets.push(s.append(who, text(id, id)).unwrap().offset);
    }
    let got: Vec<Published> = live.try_iter().collect();
    assert_eq!(
        got.iter().map(|p| (p.agent.clone(), p.seq, p.event.id.clone())).collect::<Vec<_>>(),
        [ev("a", 1, "a1"), ev("b", 1, "b1"), ev("a", 2, "a2"), ev("b", 2, "b2"), ev("a", 3, "a3")]
    );
    assert_eq!(got.iter().map(|p| p.offset).collect::<Vec<_>>(), offsets);
    assert_eq!(got[2].event, s.event_at("a", 2).unwrap().unwrap());

    // Resume `a` after seq 1: the rest of a's ledger, then only a's appends.
    let resumed = s.subscribe(Since::Agents(vec![("a".into(), 1)])).unwrap();
    s.append("b", text("b3", "b3")).unwrap();
    s.append("a", text("a4", "a4")).unwrap();
    assert_eq!(drain(&resumed), [ev("a", 2, "a2"), ev("a", 3, "a3"), ev("a", 4, "a4")]);
    assert_eq!(drain(&live), [ev("b", 3, "b3"), ev("a", 4, "a4")]);

    // From the start of everything: per agent, in seq order.
    let all = s.subscribe(Since::All).unwrap();
    assert_eq!(drain(&all), [ev("a", 1, "a1"), ev("a", 2, "a2"), ev("a", 3, "a3"), ev("a", 4, "a4"), ev("b", 1, "b1"), ev("b", 2, "b2"), ev("b", 3, "b3")]);
    // A cursor at the end replays nothing and still follows.
    let caught_up = s.subscribe(Since::Agents(vec![("b".into(), 3)])).unwrap();
    assert!(drain(&caught_up).is_empty());
    s.append("b", text("b4", "b4")).unwrap();
    assert_eq!(drain(&caught_up), [ev("b", 4, "b4")]);
    // A dropped receiver does not stop the others.
    drop(all);
    s.append("a", text("a5", "a5")).unwrap();
    assert_eq!(drain(&resumed), [ev("a", 5, "a5")]);
}

#[test]
fn one_writer_per_ledger_across_two_store_handles() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut first = Store::open(root).unwrap();
    first.create_agent(agent("a")).unwrap();
    first.append("a", text("e1", "first")).unwrap();

    let mut second = Store::open(root).unwrap();
    assert_eq!(second.agent("a").unwrap().unwrap().last_seq, 1, "the second handle reads the index");
    let refused = second.append("a", text("e2", "second")).unwrap_err();
    assert!(format!("{refused:#}").contains("locked"), "{refused:#}");
    // The refusal wrote nothing.
    assert_eq!(std::fs::read_to_string(root.join("ledgers/a.jsonl")).unwrap().lines().count(), 2);

    // A ledger nobody holds is the second handle's to write, and then the first's is refused.
    second.create_agent(agent("b")).unwrap();
    second.append("b", text("b1", "mine")).unwrap();
    assert!(first.append("b", text("b2", "theirs")).is_err());

    // Once the first handle lets go, the second may write `a`.
    drop(first);
    assert_eq!(second.append("a", text("e2", "second")).unwrap().seq, 2);
    let ids: Vec<_> = second.events("a", 0, 10).unwrap().into_iter().map(|r| r.id).collect();
    assert_eq!(ids, ["e1", "e2"]);
}

fn fixture(p: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(p)
}

#[test]
fn import_writes_a_set_into_the_store_and_reimport_adds_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path()).unwrap();
    let src = fixture("protoledger/orchestrated-peers/native");
    let r = s.import("protoledger", &src).unwrap();
    assert_eq!(r.created, ["coord-1", "peer-a", "peer-b"]);
    let set = protoledger::LedgerSet::read_path(&src).unwrap();
    let total: usize = set.iter().map(|l| l.events.len()).sum();
    assert_eq!(r.events_appended as usize, total);
    for l in set.iter() {
        // The store's ledger is the source ledger, byte for byte.
        assert_eq!(std::fs::read_to_string(s.ledger_path(l.id()).unwrap()).unwrap(), l.to_jsonl());
        assert_eq!(s.agent(l.id()).unwrap().unwrap().last_seq as usize, l.events.len());
    }
    let peers: Vec<_> = s.linked_from("coord-1", "spawned-by").unwrap().into_iter().map(|a| a.id).collect();
    assert_eq!(peers, ["peer-a", "peer-b"]);

    let again = s.import("protoledger", &src).unwrap();
    assert!(again.created.is_empty() && again.extended.is_empty());
    assert_eq!((again.events_appended, again.events_skipped as usize), (0, total));

    // A subagent is created after its parent.
    let r = s.import("auto", &fixture("protoledger/native-subagent-compaction/native")).unwrap();
    assert_eq!(r.created, ["main-1", "main-1.sub-1"]);
    let kids: Vec<_> = s.children("main-1").unwrap().into_iter().map(|a| a.id).collect();
    assert_eq!(kids, ["main-1.sub-1"]);

    // A native format.
    let r = s.import("claude-code", &fixture("claude-code/plain-chat/native")).unwrap();
    assert_eq!(r.format, "claude-code");
    assert_eq!(r.created.len(), 1);
    assert!(s.agent(&r.created[0]).unwrap().unwrap().last_seq > 0);
}

/// A ledger re-read whole (the index's record of it is not a clean
/// continuation, so `index_events` forgets the agent and re-derives its
/// binding rows) keeps the cursor of each of its bindings.
/// The cursor is the one binding column not in the ledgers: lost here, it
/// would cost every re-read binding its divergence check until the next sync.
#[test]
fn a_whole_file_re_read_keeps_its_bindings_cursors() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    {
        let s = populated(root);
        s.set_cursor("b-live", "{\"stamp\":\"live-cursor\",\"seen\":{}}").unwrap();
        s.set_cursor("b-gone", "{\"stamp\":\"gone-cursor\",\"seen\":{}}").unwrap();
    }
    let size = std::fs::metadata(root.join("ledgers/p1.jsonl")).unwrap().len();
    // The index now says p1's ledger is longer than the file: the next open
    // cannot continue from it and reads the file whole.
    poke_index(root, "UPDATE agents SET byte_len = byte_len + 100000, mtime_ns = 0 WHERE id = 'p1'");
    let s = Store::open(root).unwrap();
    let p1 = s.agent("p1").unwrap().unwrap();
    assert_eq!((p1.byte_len, p1.size), (size, size), "the whole-file read ran and re-recorded the ledger");
    assert_eq!(s.bindings().unwrap(), seeded_bindings(), "the re-read re-derived the binding rows");
    assert_eq!(s.cursor("b-live").unwrap().as_deref(), Some("{\"stamp\":\"live-cursor\",\"seen\":{}}"), "the re-read ledger's binding keeps its own cursor");
    assert_eq!(s.cursor("b-gone").unwrap().as_deref(), Some("{\"stamp\":\"gone-cursor\",\"seen\":{}}"), "another ledger's binding keeps its own");
}
