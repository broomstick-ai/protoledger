//! `protoledger store gc` (SPEC §9.5): whole ledgers removed by an
//! explicit policy, dry run by default, never an edit to a ledger. Plus the
//! roundtrip summary's `altered` count.
#![cfg(all(feature = "store", feature = "cli"))]

use protoledger::store::bindings::{Bound, Mode, bound_event, unbound_event};
use protoledger::store::gc::{Policy, archived_event, unarchived_event};
use protoledger::{Agent, Event, Kind, Ledger, LedgerSet, Link, Native, Parent, Relation, Store};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

fn agent(id: &str) -> Agent {
    Agent { id: id.into(), created_at: None, source: None, parent: None, links: vec![], meta: Default::default() }
}

fn child(id: &str, parent: &str) -> Agent {
    let mut a = agent(id);
    a.parent = Some(Parent { agent: parent.into(), event: None, relation: Relation::Subagent });
    a
}

fn text(id: &str, t: &str) -> Event {
    Event::new(id, Kind::Text).with("role", "user").with("text", t)
}

const DAY: u64 = 86_400;

/// `days` ago, as an RFC 3339 timestamp.
fn days_ago(days: u64) -> String {
    let ms = (SystemTime::now() - Duration::from_secs(days * DAY)).duration_since(SystemTime::UNIX_EPOCH).unwrap().as_millis();
    protoledger::util::ms_to_rfc3339(ms as i64)
}

/// `e` with its `ts` set `days` ago: gc ages a ledger by its newest event's
/// `ts`.
fn aged(e: Event, days: u64) -> Event {
    e.ts(Some(days_ago(days)))
}

/// One seeded agent: id, age of its last append, whether it is big.
struct Seed {
    id: &'static str,
    age_days: u64,
    big: bool,
}

const SEEDS: &[Seed] = &[
    Seed { id: "old", age_days: 100, big: false },
    Seed { id: "old-big", age_days: 100, big: true },
    Seed { id: "big", age_days: 1, big: true },
    Seed { id: "arch", age_days: 1, big: false },
    Seed { id: "arch-old", age_days: 100, big: false },
    Seed { id: "unarch", age_days: 1, big: false },
    Seed { id: "p-old", age_days: 100, big: false },
    Seed { id: "p-old.sub", age_days: 1, big: false },
    Seed { id: "keep", age_days: 1, big: false },
    Seed { id: "keep.sub", age_days: 100, big: false },
    Seed { id: "spawned", age_days: 1, big: false },
    Seed { id: "proj", age_days: 100, big: false },
];

struct Seeded {
    _dir: tempfile::TempDir,
    store: PathBuf,
    /// A native file a projection binding of `proj` names.
    projection: PathBuf,
    /// A native file an attachment binding of `proj` names.
    attachment: PathBuf,
}

fn bound(id: &str, location: &Path, mode: Mode) -> Bound {
    Bound {
        binding: id.into(),
        format: "claude-code".into(),
        variant: Some("jsonl-sidecar".into()),
        location: location.to_string_lossy().into_owned(),
        native_id: Some(format!("native-{id}")),
        mode,
    }
}

/// Twelve agents of different ages and sizes: archived, archived-then-not,
/// parents with children (an old parent with a young child, a young parent
/// with an old child), a `spawned-by` peer of an old agent, and an old agent
/// with a projection and an attachment whose native files exist.
fn seed() -> Seeded {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let native = dir.path().join("native");
    std::fs::create_dir_all(&native).unwrap();
    let projection = native.join("projected.jsonl");
    let attachment = native.join("attached.jsonl");
    std::fs::write(&projection, "{\"projected\":true}\n").unwrap();
    std::fs::write(&attachment, "{\"attached\":true}\n").unwrap();
    {
        let mut s = Store::open(&store).unwrap();
        for seed in SEEDS {
            let a = match seed.id {
                "p-old.sub" => child(seed.id, "p-old"),
                "keep.sub" => child(seed.id, "keep"),
                "spawned" => {
                    let mut a = agent(seed.id);
                    a.links = vec![Link { rel: "spawned-by".into(), agent: Some("old".into()), event: None, extra: Default::default() }];
                    a
                }
                _ => agent(seed.id),
            };
            s.create_agent(a).unwrap();
            let n = if seed.big { 40 } else { 2 };
            for i in 0..n {
                let body = if seed.big { "x".repeat(300) } else { format!("hello {i}") };
                s.append(seed.id, aged(text(&format!("{}-{i}", seed.id), &body), seed.age_days)).unwrap();
            }
        }
        let age = |id: &str| SEEDS.iter().find(|s| s.id == id).unwrap().age_days;
        for id in ["arch", "arch-old", "unarch"] {
            s.append(id, aged(archived_event(), age(id))).unwrap();
        }
        s.append("unarch", aged(unarchived_event(), age("unarch"))).unwrap();
        s.append("proj", aged(bound_event(&bound("b-proj", &projection, Mode::Projection)), age("proj"))).unwrap();
        s.append("proj", aged(bound_event(&bound("b-attach", &attachment, Mode::Attachment)), age("proj"))).unwrap();
        s.append("keep", aged(bound_event(&bound("b-keep", &native.join("keep.jsonl"), Mode::Projection)), age("keep"))).unwrap();
        s.set_cursor("b-keep", "cursor-of-keep").unwrap();
    }
    let now = SystemTime::now();
    for seed in SEEDS {
        let f = std::fs::File::options().write(true).open(store.join("ledgers").join(format!("{}.jsonl", seed.id))).unwrap();
        f.set_modified(now - Duration::from_secs(seed.age_days * DAY)).unwrap();
    }
    // Bring the index up to the new mtimes, then leave the store at rest.
    drop(Store::open(&store).unwrap());
    Seeded { _dir: dir, store, projection, attachment }
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_protoledger")).args(args).output().unwrap()
}

fn ok(args: &[&str]) -> String {
    let out = run(args);
    assert!(out.status.success(), "protoledger {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn gc(store: &Path, flags: &[&str]) -> String {
    let mut args = vec!["store", "gc", store.to_str().unwrap()];
    args.extend_from_slice(flags);
    ok(&args)
}

/// The agents a printed plan names.
fn planned(out: &str) -> BTreeSet<String> {
    out.lines().filter_map(|l| l.strip_prefix("agent\t")).map(|l| l.split('\t').next().unwrap().to_string()).collect()
}

fn set(ids: &[&str]) -> BTreeSet<String> {
    ids.iter().map(|s| s.to_string()).collect()
}

/// sha256 of every file under `root`, by relative path.
fn hashes(root: &Path) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for e in walkdir::WalkDir::new(root).sort_by_file_name() {
        let e = e.unwrap();
        if e.file_type().is_file() {
            let rel = e.path().strip_prefix(root).unwrap().to_string_lossy().into_owned();
            m.insert(rel, hex::encode(Sha256::digest(std::fs::read(e.path()).unwrap())));
        }
    }
    m
}

fn ledger(id: &str) -> String {
    format!("ledgers/{id}.jsonl")
}

// ---- (a) a dry run changes nothing ----

#[test]
fn gc_dry_run_changes_nothing_and_yes_does() {
    let s = seed();
    let before = hashes(&s.store);
    assert!(before.contains_key("index.db") && before.contains_key(&ledger("old")), "{before:?}");
    let native_before = hashes(s.projection.parent().unwrap());

    let out = gc(&s.store, &["--older-than", "30d", "--remove-projections"]);
    let expected = set(&["old", "old-big", "arch-old", "p-old", "p-old.sub", "keep.sub", "proj"]);
    assert_eq!(planned(&out), expected, "{out}");
    assert!(out.contains("dry run"), "{out}");
    assert!(out.contains("agent\tp-old.sub\tchild of p-old"), "{out}");
    assert!(out.contains(&format!("projection\t{}\tdelete", s.projection.display())), "{out}");
    assert!(out.contains("binding\tb-proj\tproj\tprojection") && out.contains("binding\tb-attach\tproj\tattachment"), "{out}");
    assert_eq!(hashes(&s.store), before, "a dry run changed the store");
    assert_eq!(hashes(s.projection.parent().unwrap()), native_before, "a dry run changed a native file");

    // Oppose: the same policy with --yes changes exactly those files.
    let out = gc(&s.store, &["--older-than", "30d", "--remove-projections", "--yes"]);
    assert_eq!(planned(&out), expected, "{out}");
    assert!(!out.contains("dry run"), "{out}");
    let after = hashes(&s.store);
    assert_ne!(after, before, "--yes changed nothing");
    for id in &expected {
        assert!(!after.contains_key(&ledger(id)), "{id} is still on disk");
    }
    assert!(!s.projection.exists(), "--remove-projections left the projected file");
    assert!(s.attachment.exists(), "an attachment's native file is never removed");
}

#[test]
fn gc_without_a_policy_refuses_and_changes_nothing() {
    let s = seed();
    let before = hashes(&s.store);
    for flags in [&["--yes"][..], &["--remove-projections", "--yes"][..], &[][..]] {
        let mut args = vec!["store", "gc", s.store.to_str().unwrap()];
        args.extend_from_slice(flags);
        let out = run(&args);
        assert!(!out.status.success(), "gc {flags:?} with no policy succeeded");
        assert!(String::from_utf8_lossy(&out.stderr).contains("needs a policy"), "{}", String::from_utf8_lossy(&out.stderr));
    }
    assert_eq!(hashes(&s.store), before);
    // Oppose: a policy is accepted.
    gc(&s.store, &["--archived"]);
}

// ---- (b) each flag selects exactly its agents; flags combine with AND ----
// ---- (d) no surviving ledger changes ----

#[test]
fn gc_each_policy_selects_exactly_its_agents_and_survivors_are_untouched() {
    let cases: &[(&[&str], &[&str])] = &[
        (&["--older-than", "30d"], &["old", "old-big", "arch-old", "p-old", "p-old.sub", "keep.sub", "proj"]),
        (&["--larger-than", "4K"], &["old-big", "big"]),
        (&["--archived"], &["arch", "arch-old"]),
        (&["--older-than", "30d", "--larger-than", "4K"], &["old-big"]),
        (&["--older-than", "30d", "--archived"], &["arch-old"]),
        (&["--larger-than", "4K", "--archived"], &[]),
        (&["--older-than", "200d"], &[]),
    ];
    for (flags, want) in cases {
        let s = seed();
        let want = set(want);
        let before = hashes(&s.store);
        let dry = gc(&s.store, flags);
        assert_eq!(planned(&dry), want, "dry run {flags:?}:\n{dry}");
        let mut yes = flags.to_vec();
        yes.push("--yes");
        let out = gc(&s.store, &yes);
        assert_eq!(planned(&out), want, "{flags:?}:\n{out}");
        let after = hashes(&s.store);
        for seed in SEEDS {
            let f = ledger(seed.id);
            if want.contains(seed.id) {
                assert!(!after.contains_key(&f), "{flags:?}: {} was selected but survives", seed.id);
            } else {
                // (d) gc never edits a ledger.
                assert_eq!(after.get(&f), before.get(&f), "{flags:?}: surviving ledger {} changed", seed.id);
            }
        }
        let s2 = Store::open(&s.store).unwrap();
        let left: BTreeSet<String> = s2.agents().unwrap().into_iter().map(|a| a.id).collect();
        let all: BTreeSet<String> = SEEDS.iter().map(|s| s.id.to_string()).collect();
        assert_eq!(left, all.difference(&want).cloned().collect(), "{flags:?}");
    }
}

#[test]
fn gc_child_goes_with_its_parent_but_not_the_reverse() {
    let s = seed();
    let out = gc(&s.store, &["--older-than", "30d", "--yes"]);
    assert!(out.contains("agent\tp-old.sub\tchild of p-old"), "{out}");
    assert!(out.contains("agent\tkeep.sub\tselected"), "{out}");
    // Children are deleted before their parents.
    let order: Vec<&str> = out.lines().filter_map(|l| l.strip_prefix("agent\t")).map(|l| l.split('\t').next().unwrap()).collect();
    let pos = |id| order.iter().position(|x| *x == id).unwrap();
    assert!(pos("p-old.sub") < pos("p-old"), "{order:?}");
    let st = Store::open(&s.store).unwrap();
    assert!(st.agent("keep").unwrap().is_some(), "a parent is kept when only its child is selected");
    assert!(st.agent("spawned").unwrap().is_some(), "a spawned-by link does not cascade");
    assert!(st.children("keep").unwrap().is_empty());
}

#[test]
fn gc_without_remove_projections_keeps_projected_files() {
    let s = seed();
    let out = gc(&s.store, &["--older-than", "30d", "--yes"]);
    assert!(out.contains(&format!("projection\t{}\tkeep", s.projection.display())), "{out}");
    assert!(s.projection.exists() && s.attachment.exists());
}

// ---- (c) after gc, a rebuilt index answers what the index answered ----

/// Every row of every index table, read straight from index.db (no store
/// open, so no catch-up can repair what gc left behind). The binding cursor
/// is left out: it is the one column a rebuild does not reproduce.
fn dump(store: &Path) -> BTreeMap<&'static str, Vec<String>> {
    let db = rusqlite::Connection::open_with_flags(store.join("index.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut m = BTreeMap::new();
    for (t, sql) in [
        ("agents", "SELECT * FROM agents ORDER BY id"),
        ("links", "SELECT * FROM links ORDER BY agent, idx"),
        ("events", "SELECT * FROM events ORDER BY agent, seq"),
        ("bindings", "SELECT binding, agent, format, variant, location, native_id, mode, active, ended FROM bindings ORDER BY binding"),
    ] {
        let mut st = db.prepare(sql).unwrap();
        let n = st.column_count();
        let rows = st
            .query_map([], |r| Ok((0..n).map(|i| format!("{:?}", r.get::<_, rusqlite::types::Value>(i).unwrap())).collect::<Vec<_>>().join("|")))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        m.insert(t, rows);
    }
    m
}

/// What the store's queries answer (agents, children, links, bindings).
fn queries(store: &Path) -> String {
    let s = Store::open(store).unwrap();
    let mut o = format!("{:?}\n{:?}\n", s.agents().unwrap(), s.bindings().unwrap());
    for seed in SEEDS {
        o += &format!(
            "{} {:?} {:?} {:?} {:?}\n",
            seed.id,
            s.agent(seed.id).unwrap(),
            s.children(seed.id).unwrap(),
            s.linked_from(seed.id, "spawned-by").unwrap(),
            s.events(seed.id, 0, usize::MAX).unwrap()
        );
    }
    o
}

#[test]
fn gc_then_reindex_reproduces_every_answer_and_removed_agents_appear_nowhere() {
    let s = seed();
    let removed = set(&["old", "old-big", "arch-old", "p-old", "p-old.sub", "keep.sub", "proj"]);
    let pre = dump(&s.store);
    assert!(pre["bindings"].iter().any(|r| r.contains("b-proj")), "sentinel: the seeded bindings are in the dump");
    gc(&s.store, &["--older-than", "30d", "--yes"]);
    let after_gc = dump(&s.store);
    let q_after_gc = queries(&s.store);

    for f in ["index.db", "index.db-wal", "index.db-shm"] {
        let _ = std::fs::remove_file(s.store.join(f));
    }
    let out = run(&["store", "reindex", s.store.to_str().unwrap()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let rebuilt = dump(&s.store);
    for t in ["agents", "links", "events", "bindings"] {
        assert_eq!(after_gc[t], rebuilt[t], "table {t}: the index after gc is not what a rebuild gives");
    }
    assert_eq!(queries(&s.store), q_after_gc);
    // Sentinel: the two dumps compared above are of different indexes, and gc changed it.
    assert_ne!(after_gc, pre, "sentinel: gc changed the index");

    // The removed agents appear nowhere: in no row's agent column, and in no query's answer.
    let agent_col = |t: &str| if t == "bindings" { 1 } else { 0 };
    for (t, rows) in &after_gc {
        for r in rows {
            let col = r.split('|').nth(agent_col(t)).unwrap();
            for id in &removed {
                assert_ne!(col, format!("Text({id:?})"), "removed agent {id} still in {t}: {r}");
            }
        }
    }
    let st = Store::open(&s.store).unwrap();
    let mut answered: BTreeSet<String> = st.agents().unwrap().into_iter().map(|a| a.id).collect();
    for a in st.agents().unwrap() {
        answered.extend(st.children(&a.id).unwrap().into_iter().map(|c| c.id));
        answered.extend(st.linked_from(&a.id, "spawned-by").unwrap().into_iter().map(|c| c.id));
    }
    answered.extend(st.bindings().unwrap().into_iter().map(|b| b.agent));
    for id in &removed {
        assert!(!answered.contains(id), "removed agent {id} still answered by a query");
        assert!(st.agent(id).unwrap().is_none() && st.events(id, 0, usize::MAX).unwrap().is_empty());
    }
    drop(st);
    // Oppose the "appears nowhere" scan: survivors do appear.
    assert!(answered.contains("keep") && answered.contains("spawned"), "{answered:?}");
    assert!(after_gc["agents"].iter().any(|r| r.starts_with("Text(\"keep\")")), "{:?}", after_gc["agents"]);
    assert!(after_gc["bindings"].iter().any(|r| r.contains("b-keep")));
}

// ---- (e) roundtrip summary: altered files are counted apart ----

#[test]
fn roundtrip_summary_counts_altered_files_in_the_denominator() {
    let dir = tempfile::tempdir().unwrap();
    let mut set = LedgerSet::new();
    let mut a = Ledger::new(agent("a"));
    let mut n = Native::new("protoledger", Some("r1".into())).record(serde_json::json!({"secret": "[redacted]"})).file(Some("a.jsonl"));
    n.altered = true;
    a.append(text("a1", "hi").native(n)).unwrap();
    set.insert(a);
    let mut b = Ledger::new(agent("b"));
    b.append(text("b1", "hello")).unwrap();
    set.insert(b);
    set.write_dir(dir.path(), false).unwrap();
    let out = ok(&["roundtrip", "protoledger", dir.path().to_str().unwrap()]);
    assert!(out.contains("files exact 1/2, altered 1;"), "{out}");
    // The carry trip's `back:` line counts them the same way.
    let via = ok(&["roundtrip", "protoledger", dir.path().to_str().unwrap(), "--via", "claude-code"]);
    assert!(via.contains("back:  files exact 1/2, altered 1;"), "{via}");
    // Oppose: with nothing altered, every file is exact and no altered count is printed.
    let plain = tempfile::tempdir().unwrap();
    let mut set = LedgerSet::new();
    for id in ["a", "b"] {
        let mut l = Ledger::new(agent(id));
        l.append(text(&format!("{id}1"), "hi")).unwrap();
        set.insert(l);
    }
    set.write_dir(plain.path(), false).unwrap();
    let out = ok(&["roundtrip", "protoledger", plain.path().to_str().unwrap()]);
    assert!(out.contains("files exact 2/2;") && !out.contains("altered"), "{out}");
    let via = ok(&["roundtrip", "protoledger", plain.path().to_str().unwrap(), "--via", "claude-code"]);
    assert!(via.contains("back:  files exact 2/2;") && !via.contains("altered"), "{via}");
}

// ---- projections, children, locking and relative locations ----

/// Set `id`'s ledger's modification time `days` ago (the clock gc ages a
/// ledger by when no event carries a `ts` and its header no `created_at`).
fn set_age(store: &Path, id: &str, days: u64) {
    let f = std::fs::File::options().write(true).open(store.join(ledger(id))).unwrap();
    f.set_modified(SystemTime::now() - Duration::from_secs(days * DAY)).unwrap();
}

/// `--remove-projections` deletes a removed agent's projected file only
/// when no surviving agent's binding, and no attachment binding at all,
/// names the same file.
#[test]
fn gc_remove_projections_never_deletes_a_file_another_binding_names() {
    let s = seed();
    let native = s.projection.parent().unwrap().to_path_buf();
    let keep_file = native.join("keep.jsonl");
    let only = native.join("only.jsonl");
    std::fs::write(&keep_file, "{\"keep\":true}\n").unwrap();
    std::fs::write(&only, "{\"only\":true}\n").unwrap();
    {
        let mut st = Store::open(&s.store).unwrap();
        // project proj -> X, unbind, then the surviving `keep` attaches X.
        st.append("proj", aged(unbound_event("b-proj"), 100)).unwrap();
        st.append("keep", bound_event(&bound("b-keep-attach", &s.projection, Mode::Attachment))).unwrap();
        // A removed agent projects to the file a surviving agent's projection names ...
        st.append("old", aged(bound_event(&bound("b-old-keep", &keep_file, Mode::Projection)), 100)).unwrap();
        // ... and to the file a removed agent's attachment names.
        st.append("old", aged(bound_event(&bound("b-old-attach", &s.attachment, Mode::Projection)), 100)).unwrap();
        // A projection nobody else names.
        st.append("proj", aged(bound_event(&bound("b-only", &only, Mode::Projection)), 100)).unwrap();
    }
    let native_before = hashes(&native);

    let flags = ["--older-than", "30d", "--remove-projections"];
    for (i, out) in [gc(&s.store, &flags), gc(&s.store, &[&flags[..], &["--yes"]].concat())].into_iter().enumerate() {
        assert!(planned(&out).contains("proj") && planned(&out).contains("old") && !planned(&out).contains("keep"), "{out}");
        for (f, by) in [(&s.projection, "b-keep-attach of keep"), (&keep_file, "b-keep of keep"), (&s.attachment, "b-attach of proj")] {
            assert!(out.contains(&format!("projection\t{}\tkeep (named by binding {by})", f.display())), "run {i}: {out}");
        }
        // Oppose: a projection named by nobody else is still deleted.
        assert!(out.contains(&format!("projection\t{}\tdelete", only.display())), "run {i}: {out}");
    }
    let native_after = hashes(&native);
    for f in [&s.projection, &keep_file, &s.attachment] {
        let rel = f.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(native_after.get(&rel), native_before.get(&rel), "{rel} was changed or deleted");
        assert!(native_after.contains_key(&rel), "{rel} was deleted");
    }
    assert!(!only.exists(), "the unshared projection was not deleted");
}

/// Removing a ledger removes its structural children (a `subagent`
/// child and a `fork` child), never an agent merely linked to it: the
/// `spawned-by` peer of a removed agent survives byte for byte.
#[test]
fn gc_removes_structural_children_but_never_a_spawned_by_peer() {
    let s = seed();
    {
        let mut st = Store::open(&s.store).unwrap();
        let mut f = agent("old.fork");
        f.parent = Some(Parent { agent: "old".into(), event: None, relation: Relation::Fork });
        st.create_agent(f).unwrap();
        st.append("old.fork", text("old.fork-0", "forked")).unwrap();
    }
    // old.fork's events carry no ts and its header no created_at: it is aged
    // by its file (1 day), so only its parent's removal takes it.
    set_age(&s.store, "old.fork", 1);
    drop(Store::open(&s.store).unwrap());
    let before = hashes(&s.store);
    let st = Store::open(&s.store).unwrap();
    let spawned = st.agent("spawned").unwrap().unwrap();
    assert_eq!(spawned.links.iter().map(|l| (l.rel.as_str(), l.agent.as_deref())).collect::<Vec<_>>(), [("spawned-by", Some("old"))]);
    assert_eq!(st.linked_from("old", "spawned-by").unwrap().len(), 1, "sentinel: the store records the link");
    drop(st);

    let out = gc(&s.store, &["--older-than", "30d", "--yes"]);
    assert!(out.contains("agent\told\tselected"), "{out}");
    assert!(out.contains("agent\told.fork\tchild of old") && out.contains("agent\tp-old.sub\tchild of p-old"), "{out}");
    assert!(!planned(&out).contains("spawned"), "{out}");
    let after = hashes(&s.store);
    for id in ["old", "old.fork", "p-old.sub"] {
        assert!(!after.contains_key(&ledger(id)), "{id} survives");
    }
    assert!(after.contains_key(&ledger("spawned")), "the spawned-by peer was removed");
    assert_eq!(after.get(&ledger("spawned")), before.get(&ledger("spawned")), "the spawned-by peer's ledger changed");
    let st = Store::open(&s.store).unwrap();
    assert_eq!(st.agent("spawned").unwrap().unwrap().links, spawned.links);
}

/// The plan is re-checked under the locks. A ledger appended to after
/// planning is kept; so is the parent of a kept child, and a parent that
/// gained a child the plan does not remove; the rest of the plan goes.
#[test]
fn gc_apply_keeps_what_changed_between_planning_and_locking() {
    let s = seed();
    let policy = Policy { older_than: Some(Duration::from_secs(30 * DAY)), ..Default::default() };
    let mut st = Store::open(&s.store).unwrap();
    let plan = st.gc_plan(&policy, SystemTime::now()).unwrap();
    assert_eq!(
        plan.agents().into_iter().collect::<BTreeSet<_>>(),
        ["arch-old", "keep.sub", "old", "old-big", "p-old", "p-old.sub", "proj"].into_iter().collect()
    );
    {
        // Another writer, between planning and locking.
        let mut other = Store::open(&s.store).unwrap();
        other.append("old", text("old-late", "late")).unwrap();
        other.append("p-old.sub", text("p-old.sub-late", "late")).unwrap();
        other.create_agent(child("arch-old.late", "arch-old")).unwrap();
    }
    let before = hashes(&s.store);
    let done = st.gc_apply(plan).unwrap();
    let skipped: BTreeMap<&str, &str> = done.skipped.iter().map(|(a, w)| (a.as_str(), w.as_str())).collect();
    assert_eq!(
        skipped,
        [
            ("arch-old", "gained child arch-old.late after the plan was made"),
            ("old", "changed after the plan was made"),
            ("p-old", "parent of kept p-old.sub"),
            ("p-old.sub", "changed after the plan was made"),
        ]
        .into_iter()
        .collect(),
        "{}",
        done.render()
    );
    // Oppose: what did not change is still removed.
    assert_eq!(done.agents().into_iter().collect::<BTreeSet<_>>(), ["keep.sub", "old-big", "proj"].into_iter().collect());
    drop(st);
    let after = hashes(&s.store);
    for id in ["keep.sub", "old-big", "proj"] {
        assert!(!after.contains_key(&ledger(id)), "{id} survives");
    }
    for id in ["arch-old", "arch-old.late", "old", "p-old", "p-old.sub"] {
        assert!(after.contains_key(&ledger(id)), "{id} was removed");
        assert_eq!(after.get(&ledger(id)), before.get(&ledger(id)), "{id} changed");
    }
    let st = Store::open(&s.store).unwrap();
    assert_eq!(st.agent("arch-old.late").unwrap().unwrap().parent.unwrap().agent, "arch-old");
}

/// A gc that cannot lock every planned ledger writes nothing to any of
/// them, not even the writer's torn-tail repair on one it locked first.
#[test]
fn gc_that_cannot_lock_every_ledger_leaves_a_torn_tail_alone() {
    let s = seed();
    {
        use std::io::Write;
        let mut f = std::fs::File::options().append(true).open(s.store.join(ledger("old"))).unwrap();
        f.write_all(b"{\"torn").unwrap();
    }
    drop(Store::open(&s.store).unwrap());
    let before = hashes(&s.store.join("ledgers"));
    assert!(std::fs::read(s.store.join(ledger("old"))).unwrap().ends_with(b"{\"torn"), "sentinel: the torn tail is there");
    // `old` is locked before `proj` (same depth, by id).
    let held = std::fs::File::open(s.store.join(ledger("proj"))).unwrap();
    held.try_lock().unwrap();
    let out = run(&["store", "gc", s.store.to_str().unwrap(), "--older-than", "30d", "--yes"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("proj is in use; nothing was removed"), "{err}");
    assert_eq!(hashes(&s.store.join("ledgers")), before, "a refused gc changed a ledger");
    drop(held);
    // Oppose: once the lock is free, the same gc removes them.
    let out = gc(&s.store, &["--older-than", "30d", "--yes"]);
    assert!(planned(&out).contains("old") && !s.store.join(ledger("old")).exists() && !s.store.join(ledger("proj")).exists(), "{out}");
}

/// A relative binding location is never resolved (against the cwd or
/// anything else) and never deleted.
#[test]
fn gc_never_resolves_or_deletes_a_relative_projection_location() {
    let s = seed();
    let cwd = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(cwd.path().join("rel")).unwrap();
    let rel_file = cwd.path().join("rel/relative.jsonl");
    std::fs::write(&rel_file, "{\"relative\":true}\n").unwrap();
    {
        let mut st = Store::open(&s.store).unwrap();
        st.append("proj", aged(bound_event(&bound("b-rel", Path::new("rel/relative.jsonl"), Mode::Projection)), 100)).unwrap();
    }
    let out = Command::new(env!("CARGO_BIN_EXE_protoledger"))
        .current_dir(cwd.path())
        .args(["store", "gc", s.store.to_str().unwrap(), "--older-than", "30d", "--remove-projections", "--yes"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains("projection\trel/relative.jsonl\tkeep (relative location"), "{stdout}");
    assert!(rel_file.exists(), "a relative location was resolved against the cwd and deleted");
    // Oppose: in the same run the absolute projection is deleted.
    assert!(stdout.contains(&format!("projection\t{}\tdelete", s.projection.display())) && !s.projection.exists(), "{stdout}");
}

/// A SURVIVING agent attaches a planned
/// projection's file between `gc_plan` and `gc_apply`, and nothing planned
/// changed (so no agent is kept). Under the locks gc re-places projections
/// anyway: the newly attached file is kept, and an unshared planned
/// projection is still deleted (opposes keep-everything).
#[test]
fn gc_apply_keeps_a_projection_attached_between_plan_and_apply() {
    let d = tempfile::tempdir().unwrap();
    let store = d.path().join("store");
    let p = d.path().join("native/p.jsonl");
    let q = d.path().join("native/q.jsonl");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, "{}\n").unwrap();
    std::fs::write(&q, "{}\n").unwrap();
    {
        let mut s = Store::open(&store).unwrap();
        for id in ["a", "c", "young"] {
            s.create_agent(agent(id)).unwrap();
            s.append(id, text(&format!("{id}-0"), "hi")).unwrap();
        }
        s.append("a", aged(bound_event(&bound("b-a", &p, Mode::Projection)), 100)).unwrap();
        s.append("c", aged(bound_event(&bound("b-c", &q, Mode::Projection)), 100)).unwrap();
    }
    let policy = Policy { older_than: Some(Duration::from_secs(30 * DAY)), remove_projections: true, ..Default::default() };
    let mut st = Store::open(&store).unwrap();
    let plan = st.gc_plan(&policy, SystemTime::now()).unwrap();
    assert_eq!(plan.agents(), ["a", "c"]);
    assert_eq!(plan.projections_removed.iter().collect::<BTreeSet<_>>(), [&p, &q].into_iter().collect(), "sentinel: both files are planned for deletion");
    {
        // Another writer: the surviving `young` attaches p.
        let mut other = Store::open(&store).unwrap();
        other.append("young", bound_event(&bound("b-young", &p, Mode::Attachment))).unwrap();
    }
    let done = st.gc_apply(plan).unwrap();
    assert!(done.skipped.is_empty(), "sentinel: no agent kept, so this is the attached-between-plan-and-apply path\n{}", done.render());
    assert_eq!(done.agents(), ["a", "c"]);
    assert!(p.exists(), "gc deleted a file a surviving agent attached after the plan was made:\n{}", done.render());
    assert!(done.projections_kept.iter().any(|(k, w)| k == &p && w == "named by binding b-young of young"), "{}", done.render());
    // Oppose: the projection nobody else names is still deleted.
    assert!(!q.exists(), "the unshared projection survived:\n{}", done.render());
    assert_eq!(done.projections_removed, vec![q.clone()]);
}

// ---- the gc clock ----

/// The `clock` lines of a printed plan: agent → (label, time).
fn clocks(out: &str) -> BTreeMap<String, (String, String)> {
    out.lines()
        .filter_map(|l| l.strip_prefix("clock\t"))
        .map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            (f[0].to_string(), (f[1].to_string(), f[2].to_string()))
        })
        .collect()
}

/// Set every file under `root`'s modification time to now, as a copy that
/// does not preserve times leaves them.
fn touch_all(root: &Path) {
    let now = SystemTime::now();
    for e in walkdir::WalkDir::new(root) {
        let e = e.unwrap();
        if e.file_type().is_file() {
            std::fs::File::options().write(true).open(e.path()).unwrap().set_modified(now).unwrap();
        }
    }
}

/// GC ages a ledger by its newest event's `ts`, which survives a copy
/// of the store. A store copied with every mtime reset to now still collects
/// the ledger whose newest event is old (the mtime clock would collect
/// nothing), and never the one whose newest event is recent.
#[test]
fn gc_on_a_copied_store_still_collects_a_ledger_whose_newest_event_is_old() {
    let s = seed();
    let copy_dir = tempfile::tempdir().unwrap();
    let copy = copy_dir.path().join("store");
    for e in walkdir::WalkDir::new(&s.store) {
        let e = e.unwrap();
        let to = copy.join(e.path().strip_prefix(&s.store).unwrap());
        if e.file_type().is_dir() {
            std::fs::create_dir_all(&to).unwrap();
        } else {
            std::fs::copy(e.path(), &to).unwrap();
        }
    }
    touch_all(&copy);
    // Sentinel: every ledger in the copy is young by its file, so an mtime clock selects nothing.
    for e in std::fs::read_dir(copy.join("ledgers")).unwrap() {
        let age = SystemTime::now().duration_since(e.unwrap().metadata().unwrap().modified().unwrap()).unwrap_or_default();
        assert!(age < Duration::from_secs(DAY), "sentinel: a copied ledger kept an old mtime");
    }
    let out = gc(&copy, &["--older-than", "30d"]);
    assert_eq!(planned(&out), set(&["old", "old-big", "arch-old", "p-old", "p-old.sub", "keep.sub", "proj"]), "{out}");
    let c = clocks(&out);
    // Who sits in each rung. Every seeded agent is aged by an event's ts.
    assert_eq!(c.len(), SEEDS.len(), "{out}");
    assert!(c.values().all(|(label, _)| label == "newest-event-ts"), "{out}");
    // Opposed: the young ledgers are not collected.
    for id in ["big", "arch", "unarch", "keep", "spawned"] {
        assert!(!planned(&out).contains(id), "{id} collected: {out}");
    }
    // And --yes on the copy removes exactly what the dry run named.
    let done = gc(&copy, &["--older-than", "30d", "--yes"]);
    assert_eq!(planned(&done), planned(&out));
    assert!(!copy.join(ledger("old")).exists() && copy.join(ledger("keep")).exists());
}

/// A ledger whose file is old but whose newest event is recent is NOT
/// collected (its events say it is in use); opposed in the same run by a
/// ledger whose newest event is old and whose file is new, which is.
#[test]
fn gc_does_not_collect_an_old_file_with_a_recent_newest_event() {
    let d = tempfile::tempdir().unwrap();
    let store = d.path().join("store");
    {
        let mut s = Store::open(&store).unwrap();
        s.create_agent(agent("fresh")).unwrap();
        s.append("fresh", aged(text("fresh-0", "a"), 100)).unwrap();
        s.append("fresh", aged(text("fresh-1", "b"), 1)).unwrap();
        // An event with no ts after it does not hide the newest one that has one.
        s.append("fresh", text("fresh-2", "c")).unwrap();
        s.create_agent(agent("stale")).unwrap();
        s.append("stale", aged(text("stale-0", "a"), 200)).unwrap();
        s.append("stale", aged(text("stale-1", "b"), 100)).unwrap();
    }
    set_age(&store, "fresh", 100);
    drop(Store::open(&store).unwrap());
    let out = gc(&store, &["--older-than", "30d"]);
    assert_eq!(planned(&out), set(&["stale"]), "{out}");
    let c = clocks(&out);
    assert_eq!(c["fresh"].0, "newest-event-ts", "{out}");
    assert_eq!(c["fresh"].1[..10], days_ago(1)[..10], "fresh is aged by its 1-day-old event, not its 100-day-old one: {out}");
    assert_eq!(c["stale"].0, "newest-event-ts", "{out}");
}

/// The clock's fallbacks, each reached by a ledger built to need it, with the
/// dry run's clock label asserted for every agent in the store (the population of each rung is named, and each rung has a young agent
/// that it keeps, so a rung that always selects or never selects fails).
#[test]
fn gc_falls_back_to_created_at_then_mtime_and_names_the_clock() {
    let d = tempfile::tempdir().unwrap();
    let store = d.path().join("store");
    let with_created = |id: &str, days: u64| {
        let mut a = agent(id);
        a.created_at = Some(days_ago(days));
        a
    };
    {
        let mut s = Store::open(&store).unwrap();
        // Rung 1: an event's ts (beats an old created_at and a new mtime).
        s.create_agent(with_created("ev-old", 1)).unwrap();
        s.append("ev-old", aged(text("ev-old-0", "x"), 100)).unwrap();
        s.create_agent(with_created("ev-young", 100)).unwrap();
        s.append("ev-young", aged(text("ev-young-0", "x"), 1)).unwrap();
        // Rung 2: no event has a ts; the header's created_at (beats the mtime).
        s.create_agent(with_created("created-old", 100)).unwrap();
        s.append("created-old", text("created-old-0", "x")).unwrap();
        s.create_agent(with_created("created-young", 1)).unwrap();
        s.append("created-young", text("created-young-0", "x")).unwrap();
        // Rung 3: neither; the file's mtime.
        s.create_agent(agent("mtime-old")).unwrap();
        s.append("mtime-old", text("mtime-old-0", "x")).unwrap();
        s.create_agent(agent("mtime-young")).unwrap();
        s.append("mtime-young", text("mtime-young-0", "x")).unwrap();
        // A ts that is not a time is no clock: the next rung decides.
        s.create_agent(with_created("bad-ts", 100)).unwrap();
        s.append("bad-ts", text("bad-ts-0", "x").ts(Some("yesterday"))).unwrap();
    }
    set_age(&store, "created-young", 100);
    set_age(&store, "mtime-old", 100);
    drop(Store::open(&store).unwrap());
    let out = gc(&store, &["--older-than", "30d"]);
    let labels: BTreeMap<String, String> = clocks(&out).into_iter().map(|(a, (l, _))| (a, l)).collect();
    let want: BTreeMap<String, String> = [
        ("ev-old", "newest-event-ts"),
        ("ev-young", "newest-event-ts"),
        ("created-old", "header-created-at"),
        ("created-young", "header-created-at"),
        ("bad-ts", "header-created-at"),
        ("mtime-old", "file-mtime"),
        ("mtime-young", "file-mtime"),
    ]
    .into_iter()
    .map(|(a, l)| (a.to_string(), l.to_string()))
    .collect();
    assert_eq!(labels, want, "{out}");
    assert_eq!(planned(&out), set(&["ev-old", "created-old", "bad-ts", "mtime-old"]), "{out}");
}
