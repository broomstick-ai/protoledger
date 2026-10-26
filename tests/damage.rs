//! One damaged ledger never stops a store from opening (SPEC §10.6): the
//! index skips it and the store names it, once; every other ledger reads
//! byte for byte; and every operation on the damaged one (`cat`, an append)
//! fails naming it, through the library and through the binary. Nothing in
//! it is changed. Three kinds of damage: a compressed ledger whose leftover
//! hot tail contradicts its blocks, a compressed ledger with a corrupt block
//! that is not its last, and a plain ledger that does not parse.
#![cfg(all(feature = "store", feature = "cli"))]

use protoledger::store::Store;
use protoledger::{Agent, Event, Kind};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

fn agent(id: &str) -> Agent {
    Agent { id: id.into(), created_at: None, source: None, parent: None, links: vec![], meta: Default::default() }
}

fn text(id: &str, t: &str) -> Event {
    Event::new(id, Kind::Text).with("role", "user").with("text", t)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Damage {
    /// Compressed: the hot tail holds the ledger's last line, already in the
    /// last block, with one byte changed (a leftover tail that does not match).
    LeftoverContradicts,
    /// Compressed: a byte flipped inside a block that is not the last.
    CorruptMiddleBlock,
    /// Plain: a line in the middle of the ledger is not JSON.
    PlainUnparsable,
}

/// A store at `root` with agents `a`, `b` and `c`, compressed (every append
/// sealed into its own block) or plain; returns each ledger's stream.
fn build(root: &Path, compressed: bool) -> BTreeMap<String, Vec<u8>> {
    let mut s = Store::open(root).unwrap();
    if compressed {
        s.set_compression(true).unwrap();
        s.set_seal_bytes(1).unwrap();
    }
    for id in ["a", "b", "c"] {
        s.create_agent(agent(id)).unwrap();
        for n in 0..5 {
            s.append(id, text(&format!("{id}{n}"), &format!("line {n} of {id}"))).unwrap();
        }
    }
    let streams = ["a", "b", "c"].iter().map(|id| (id.to_string(), s.reader(id).unwrap().stream().unwrap())).collect();
    drop(s);
    let ledgers = root.join("ledgers");
    let names: Vec<String> = std::fs::read_dir(&ledgers).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    // Sentinel: each store is in the form the case is about.
    assert_eq!(names.iter().any(|n| n.ends_with(".jsonl.zst")), compressed, "{names:?}");
    assert_eq!(names.iter().any(|n| n.ends_with(".jsonl")), !compressed, "{names:?}");
    streams
}

/// The meta frames of a block file: (position, total length of the block).
fn blocks(zst: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < zst.len() {
        let n = u32::from_le_bytes(zst[i + 4..i + 8].try_into().unwrap()) as usize;
        let meta: serde_json::Value = serde_json::from_slice(&zst[i + 8..i + 8 + n]).unwrap();
        let total = 8 + n + meta["zlen"].as_u64().unwrap() as usize;
        out.push((i, total));
        i += total;
    }
    out
}

/// Damage `a`'s ledger at `root`.
fn damage(root: &Path, kind: Damage, stream_a: &[u8]) {
    let d = root.join("ledgers");
    match kind {
        Damage::LeftoverContradicts => {
            let tail = d.join("a.jsonl.tail");
            assert_eq!(std::fs::metadata(&tail).unwrap().len(), 0, "sentinel: every line is sealed");
            let last = stream_a[..stream_a.len() - 1].rsplit(|b| *b == b'\n').next().unwrap();
            let bad = String::from_utf8(last.to_vec()).unwrap().replace("line 4 of a", "line X of a") + "\n";
            assert_ne!(bad.as_bytes(), &stream_a[stream_a.len() - last.len() - 1..]);
            std::fs::write(&tail, bad).unwrap();
        }
        Damage::CorruptMiddleBlock => {
            let p = d.join("a.jsonl.zst");
            let mut zst = std::fs::read(&p).unwrap();
            let b = blocks(&zst);
            assert!(b.len() >= 3, "{} blocks", b.len());
            let (at, total) = b[1];
            zst[at + total - 6] ^= 0x55;
            std::fs::write(&p, zst).unwrap();
        }
        Damage::PlainUnparsable => {
            let p = d.join("a.jsonl");
            let text = String::from_utf8(std::fs::read(&p).unwrap()).unwrap();
            let mut lines: Vec<&str> = text.lines().collect();
            lines[3] = "{this line is not json";
            std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        }
    }
}

/// Every file under the store's ledgers/, by name.
fn files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(root.join("ledgers"))
        .unwrap()
        .map(|e| e.unwrap())
        .map(|e| (e.file_name().to_string_lossy().into_owned(), std::fs::read(e.path()).unwrap()))
        .collect()
}

fn bin(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_protoledger")).args(args).output().unwrap()
}

/// The case: build, damage `a`, then open with the index `fresh` (deleted
/// first, so catch-up reads every ledger whole) or as it was.
fn case(kind: Damage, fresh: bool) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    let streams = build(&root, kind != Damage::PlainUnparsable);
    damage(&root, kind, &streams["a"]);
    if fresh {
        std::fs::remove_file(root.join("index.db")).unwrap();
    }
    let before = files(&root);
    let what = format!("{kind:?} (fresh index: {fresh})");

    // The store opens, and names the damaged ledger once.
    let mut s = Store::open(&root).unwrap_or_else(|e| panic!("{what}: the store did not open: {e:#}"));
    let damaged = s.damaged();
    assert_eq!(damaged.iter().map(|d| d.ledger.as_str()).collect::<Vec<_>>(), ["ledgers/a.jsonl"], "{what}: {damaged:?}");
    // Its agent, when the index or its header can say (a header inside a
    // damaged block cannot).
    assert!(damaged[0].agent.as_deref().is_none_or(|a| a == "a"), "{what}: {damaged:?}");
    assert!(!damaged[0].reason.is_empty(), "{what}");
    // Every other ledger reads byte for byte, and its events are indexed.
    for id in ["b", "c"] {
        assert!(s.reader(id).unwrap().stream().unwrap() == streams[id], "{what}: {id}");
        assert_eq!(s.agent(id).unwrap().unwrap().last_seq, 5, "{what}: {id}");
        assert_eq!(s.event_at(id, 5).unwrap().unwrap().id, format!("{id}4"), "{what}: {id}");
    }
    // Every operation on the damaged one fails, naming it.
    let e = format!("{:#}", s.reader("a").unwrap_err());
    assert!(e.contains("a.jsonl") && e.contains("damaged"), "{what}: cat: {e}");
    let e = format!("{:#}", s.append("a", text("a9", "more")).unwrap_err());
    assert!(e.contains("a.jsonl") && e.contains("damaged"), "{what}: append: {e}");
    let e = format!("{:#}", s.held("a").map(|_| ()).unwrap_err());
    assert!(e.contains("a.jsonl"), "{what}: held: {e}");
    // Still once after another catch-up and after a rebuild of the index.
    s.refresh().unwrap();
    assert_eq!(s.damaged().len(), 1, "{what}: after refresh: {:?}", s.damaged());
    s.reindex().unwrap();
    assert_eq!(s.damaged().iter().map(|d| d.ledger.as_str()).collect::<Vec<_>>(), ["ledgers/a.jsonl"], "{what}: after reindex");
    assert!(s.reader("b").unwrap().stream().unwrap() == streams["b"], "{what}: b after reindex");
    // Healthy ledgers still take appends.
    s.append("b", text("b9", "more")).unwrap();
    drop(s);

    // The binary: `cat` of the damaged ledger fails naming it; `cat` of a
    // healthy one writes its stream, and says which ledger it skipped.
    let st = root.to_str().unwrap();
    let out = bin(&["cat", st, "a"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && out.stdout.is_empty(), "{what}: cat a succeeded");
    assert!(err.contains("a.jsonl") && err.contains("damaged"), "{what}: cat a: {err}");
    let out = bin(&["cat", st, "c"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{what}: cat c: {err}");
    assert!(out.stdout == streams["c"], "{what}: cat c is not byte for byte");
    let named: Vec<&str> = err.lines().filter(|l| l.contains("is damaged and was skipped")).collect();
    assert_eq!(named.len(), 1, "{what}: the binary names it once: {err}");
    assert!(named[0].contains("ledger ledgers/a.jsonl "), "{what}: {err}");
    let out = bin(&["store", "ls", st]);
    assert!(out.status.success(), "{what}: ls: {}", String::from_utf8_lossy(&out.stderr));

    // Nothing in the damaged ledger was changed or deleted.
    let after = files(&root);
    for (n, bytes) in &before {
        if n.starts_with("a.") {
            assert!(after.get(n) == Some(bytes), "{what}: {n} changed");
        }
    }
    assert_eq!(after.keys().filter(|n| n.starts_with("a.")).collect::<Vec<_>>(), before.keys().filter(|n| n.starts_with("a.")).collect::<Vec<_>>(), "{what}");
}

#[test]
fn a_leftover_tail_that_contradicts_its_blocks_is_skipped_named_once_and_refused() {
    case(Damage::LeftoverContradicts, true);
    case(Damage::LeftoverContradicts, false);
}

#[test]
fn a_corrupt_block_that_is_not_the_last_is_skipped_named_once_and_refused() {
    case(Damage::CorruptMiddleBlock, true);
    case(Damage::CorruptMiddleBlock, false);
}

#[test]
fn a_plain_ledger_that_does_not_parse_is_skipped_named_once_and_refused() {
    case(Damage::PlainUnparsable, true);
    case(Damage::PlainUnparsable, false);
}

/// Opposed: a store with nothing damaged names nothing, and its ledgers
/// open for appending (so the refusal above is about the damage).
#[test]
fn a_store_with_nothing_damaged_names_nothing() {
    for compressed in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");
        let streams = build(&root, compressed);
        std::fs::remove_file(root.join("index.db")).unwrap();
        let mut s = Store::open(&root).unwrap();
        assert!(s.damaged().is_empty(), "{:?}", s.damaged());
        assert!(s.reader("a").unwrap().stream().unwrap() == streams["a"]);
        s.append("a", text("a9", "more")).unwrap();
        let out = bin(&["cat", root.to_str().unwrap(), "a"]);
        assert!(out.status.success() && !String::from_utf8_lossy(&out.stderr).contains("damaged"));
    }
}

// ---- the rest of the store in front of a damaged ledger ----
//
// A damaged ledger whose index rows predate the damage (the index read it
// before it was damaged) is the case every command below meets in an
// ordinary store; one whose index never read it (index.db rebuilt since) is
// the other. Each is put in front of garbage collection (dry run and
// `--yes`), a subscription, and the commands that do not open the store for
// writing; each is opposed by the same store with nothing damaged.

/// Every file of the ledger named `name` (`a` for `ledgers/a.jsonl...`).
fn ledger_files(all: &BTreeMap<String, Vec<u8>>, name: &str) -> BTreeMap<String, Vec<u8>> {
    all.iter().filter(|(n, _)| n.split(".jsonl").next() == Some(name)).map(|(n, b)| (n.clone(), b.clone())).collect()
}

/// The agents a printed gc plan removes.
fn planned(out: &str) -> Vec<String> {
    out.lines().filter_map(|l| l.strip_prefix("agent\t")).map(|l| l.split('\t').next().unwrap().to_string()).collect()
}

/// gc, dry run then `--yes`, with a policy that selects every ledger:
/// the damaged one is never selected, named in the plan and on stderr, and
/// left exactly as it was; the others are removed.
#[test]
fn gc_never_removes_a_damaged_ledger_and_names_it_in_the_dry_run_and_with_yes() {
    for kind in [Damage::LeftoverContradicts, Damage::CorruptMiddleBlock, Damage::PlainUnparsable] {
        for fresh in [false, true] {
            let what = format!("{kind:?} (fresh index: {fresh})");
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("store");
            let streams = build(&root, kind != Damage::PlainUnparsable);
            damage(&root, kind, &streams["a"]);
            if fresh {
                std::fs::remove_file(root.join("index.db")).unwrap();
            }
            let st = root.to_str().unwrap();
            let before = files(&root);
            // The dry run: changes nothing, plans b and c, names a.
            let out = bin(&["store", "gc", st, "--larger-than", "1"]);
            let (o, e) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            assert!(out.status.success(), "{what}: dry run: {e}");
            assert_eq!(planned(&o), ["b", "c"], "{what}: dry run: {o}");
            assert!(o.contains("damaged\tledgers/a.jsonl\tnever removed: "), "{what}: dry run: {o}");
            assert_eq!(
                e.lines().filter(|l| l.contains("ledger ledgers/a.jsonl") && l.contains("is damaged and was skipped")).count(),
                1,
                "{what}: dry run stderr: {e}"
            );
            assert!(files(&root) == before, "{what}: the dry run changed a file");
            // --yes: removes b and c, never a.
            let out = bin(&["store", "gc", st, "--larger-than", "1", "--yes"]);
            let (o, e) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            assert!(out.status.success(), "{what}: --yes: {e}");
            assert_eq!(planned(&o), ["b", "c"], "{what}: --yes: {o}");
            assert!(o.contains("damaged\tledgers/a.jsonl\tnever removed: "), "{what}: --yes: {o}");
            assert_eq!(e.lines().filter(|l| l.contains("is damaged and was skipped")).count(), 1, "{what}: --yes stderr: {e}");
            let after = files(&root);
            assert!(
                ledger_files(&after, "a") == ledger_files(&before, "a") && !ledger_files(&before, "a").is_empty(),
                "{what}: gc changed or removed the damaged ledger: {:?}",
                after.keys()
            );
            assert!(ledger_files(&after, "b").is_empty() && ledger_files(&after, "c").is_empty(), "{what}: {:?}", after.keys());
            // And it is still named, and still refused.
            let s = Store::open(&root).unwrap();
            assert_eq!(s.damaged().iter().map(|d| d.ledger.as_str()).collect::<Vec<_>>(), ["ledgers/a.jsonl"], "{what}");
            assert!(s.reader("a").is_err(), "{what}");
        }
    }
}

/// Opposed: with nothing damaged, the same policy removes every ledger, and
/// nothing is named damaged.
#[test]
fn gc_of_a_store_with_nothing_damaged_removes_every_selected_ledger() {
    for compressed in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");
        build(&root, compressed);
        let st = root.to_str().unwrap();
        let out = bin(&["store", "gc", st, "--larger-than", "1"]);
        let (o, e) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success() && planned(&o) == ["a", "b", "c"] && !o.contains("damaged") && !e.contains("damaged"), "{o}\n{e}");
        let out = bin(&["store", "gc", st, "--larger-than", "1", "--yes"]);
        let (o, e) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success() && planned(&o) == ["a", "b", "c"] && !o.contains("damaged") && !e.contains("damaged"), "{o}\n{e}");
        assert!(files(&root).is_empty(), "{:?}", files(&root).keys());
    }
}

/// A plan made while every ledger was healthy, applied after one was
/// damaged: the apply catches up under its locks, finds the damage, and
/// keeps that ledger (named in `skipped`); the others go.
#[test]
fn gc_apply_keeps_a_ledger_damaged_after_the_plan_was_made() {
    for kind in [Damage::LeftoverContradicts, Damage::CorruptMiddleBlock, Damage::PlainUnparsable] {
        let what = format!("{kind:?}");
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");
        let streams = build(&root, kind != Damage::PlainUnparsable);
        let mut s = Store::open(&root).unwrap();
        let policy = protoledger::store::GcPolicy { larger_than: Some(1), ..Default::default() };
        let plan = s.gc_plan(&policy, std::time::SystemTime::now()).unwrap();
        assert_eq!(plan.agents(), ["a", "b", "c"], "{what}: sentinel: the plan removes a");
        let before = files(&root);
        damage(&root, kind, &streams["a"]);
        let damaged = ledger_files(&files(&root), "a");
        assert!(damaged != ledger_files(&before, "a"), "{what}: sentinel: a was damaged");
        let done = s.gc_apply(plan).unwrap();
        assert_eq!(done.agents(), ["b", "c"], "{what}: {done:?}");
        assert!(done.skipped.iter().any(|(id, why)| id == "a" && why.contains("damaged")), "{what}: {:?}", done.skipped);
        assert!(ledger_files(&files(&root), "a") == damaged, "{what}: gc changed or removed the damaged ledger");
    }
}

/// A child whose ledger is damaged keeps its parent: removing the parent
/// would leave the child naming a ledger that does not exist (SPEC §2.1),
/// whether the index or only the child's header names the parent. Opposed:
/// healthy, the child goes with its parent.
#[test]
fn gc_keeps_the_parent_of_a_damaged_child() {
    // (damaged, the index rebuilt after the damage: the parent is named only
    // by the damaged ledger's header)
    for (damaged, fresh) in [(true, false), (true, true), (false, false)] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");
        let mut s = Store::open(&root).unwrap();
        s.create_agent(agent("p")).unwrap();
        let mut c = agent("p.sub");
        c.parent = Some(protoledger::Parent { agent: "p".into(), event: None, relation: protoledger::Relation::Subagent });
        s.create_agent(c).unwrap();
        for n in 0..5 {
            s.append("p", text(&format!("p{n}"), "parent")).unwrap();
            s.append("p.sub", text(&format!("s{n}"), "child")).unwrap();
        }
        let child = root.join(&s.agent("p.sub").unwrap().unwrap().path);
        drop(s);
        if damaged {
            let text = String::from_utf8(std::fs::read(&child).unwrap()).unwrap();
            let mut lines: Vec<&str> = text.lines().collect();
            lines[3] = "{this line is not json";
            std::fs::write(&child, lines.join("\n") + "\n").unwrap();
        }
        if fresh {
            std::fs::remove_file(root.join("index.db")).unwrap();
        }
        let out = bin(&["store", "gc", root.to_str().unwrap(), "--larger-than", "1", "--yes"]);
        let o = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        if damaged {
            assert!(planned(&o).is_empty(), "{o}");
            assert!(o.contains("kept\tp\tparent of damaged p.sub"), "{o}");
            assert!(child.exists() && root.join("ledgers/p.jsonl").exists(), "{o}");
        } else {
            assert_eq!(planned(&o), ["p.sub", "p"], "{o}");
            assert!(!child.exists() && !root.join("ledgers/p.jsonl").exists(), "{o}");
        }
    }
}

/// A subscription to every agent leaves the damaged ledger out and sends
/// every other ledger's events; one naming the damaged ledger is refused,
/// naming it. Opposed: healthy, every agent's events come.
#[test]
fn subscribe_all_skips_a_damaged_ledger_and_a_subscription_naming_it_is_refused() {
    use protoledger::store::Since;
    for kind in [Damage::LeftoverContradicts, Damage::CorruptMiddleBlock, Damage::PlainUnparsable] {
        for fresh in [false, true] {
            let what = format!("{kind:?} (fresh index: {fresh})");
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("store");
            let streams = build(&root, kind != Damage::PlainUnparsable);
            damage(&root, kind, &streams["a"]);
            if fresh {
                std::fs::remove_file(root.join("index.db")).unwrap();
            }
            let s = Store::open(&root).unwrap();
            let rx = s.subscribe(Since::All).unwrap_or_else(|e| panic!("{what}: subscribe(All) failed: {e:#}"));
            let got: Vec<(String, u64)> = rx.try_iter().map(|p| (p.agent, p.seq)).collect();
            let want: Vec<(String, u64)> = ["b", "c"].iter().flat_map(|a| (1..=5).map(move |q| (a.to_string(), q))).collect();
            assert_eq!(got, want, "{what}");
            let e = format!("{:#}", s.subscribe(Since::Agents(vec![("a".into(), 0)])).unwrap_err());
            assert!(e.contains("a.jsonl") && e.contains("damaged"), "{what}: {e}");
            assert!(s.subscribe(Since::Agents(vec![("b".into(), 2)])).unwrap().try_iter().count() == 3, "{what}");
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    build(&root, true);
    let s = Store::open(&root).unwrap();
    assert_eq!(s.subscribe(Since::All).unwrap().try_iter().count(), 15);
    assert_eq!(s.subscribe(Since::Agents(vec![("a".into(), 0)])).unwrap().try_iter().count(), 5);
}

/// The commands that do not open the store as the others do (`store gc`'s
/// dry run, `store dict add|train|rm`) name a damaged ledger on stderr, once;
/// `store ls` marks its stale row. Opposed: healthy, none says "damaged".
#[test]
fn dict_commands_and_ls_name_a_damaged_ledger() {
    for damaged in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");
        let streams = build(&root, true);
        // Enough lines in the healthy ledgers to train on.
        let mut s = Store::open(&root).unwrap();
        for id in ["b", "c"] {
            for n in 10..40 {
                s.append(id, text(&format!("{id}{n}"), &format!("line {n} of {id}, with some words to sample"))).unwrap();
            }
        }
        drop(s);
        if damaged {
            damage(&root, Damage::CorruptMiddleBlock, &streams["a"]);
        }
        let st = root.to_str().unwrap();
        let dict = dir.path().join("d.zdict");
        std::fs::write(&dict, std::fs::read(root.join("ledgers/b.jsonl.zst")).unwrap().repeat(8)).unwrap();
        let mut runs: Vec<(Vec<&str>, std::process::Output)> = Vec::new();
        let train = bin(&["store", "dict", "train", st, "--size", "1024"]);
        let added = bin(&["store", "dict", "add", st, dict.to_str().unwrap()]);
        let id = String::from_utf8_lossy(&added.stdout).trim().to_string();
        runs.push((vec!["dict", "train"], train));
        runs.push((vec!["dict", "add"], added));
        runs.push((vec!["dict", "rm"], bin(&["store", "dict", "rm", st, &id])));
        for (cmd, out) in &runs {
            let e = String::from_utf8_lossy(&out.stderr);
            let named = e.lines().filter(|l| l.contains("ledger ledgers/a.jsonl") && l.contains("is damaged and was skipped")).count();
            assert_eq!(named, usize::from(damaged), "{cmd:?} (damaged: {damaged}): {e}");
            if !damaged {
                assert!(!e.contains("damaged"), "{cmd:?}: {e}");
            }
            // A damaged ledger is no sample, and stops nothing else.
            assert!(out.status.success(), "{cmd:?} (damaged: {damaged}): {e}");
        }
        let out = bin(&["store", "ls", st]);
        let o = String::from_utf8_lossy(&out.stdout);
        let a: Vec<&str> = o.lines().filter(|l| l.starts_with("a\t")).collect();
        assert_eq!(a.len(), 1, "{o}");
        assert_eq!(a[0].contains("damaged"), damaged, "{o}");
        assert_eq!(a[0].contains("events="), !damaged, "{o}");
    }
}
