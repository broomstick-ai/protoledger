//! The read cache never answers what the ledgers on disk do not hold
//! (SPEC §9.6), whatever changed them: appends, redaction, garbage
//! collection and a ledger made again under the same id (by this handle,
//! another handle or another process), recompression, migration, and a
//! rewrite in place by a foreign process that the cache can tell from an
//! append. Each test compares the cached store's answer with the disk read
//! without any cache (a reader made from the path alone, or a fresh export).
//! The one rewrite it cannot tell from an append is pinned by
//! `a_growing_rewrite_away_from_the_old_end_is_the_stated_limit`.

use protoledger::formats::{self, ExportOptions};
use protoledger::store::{GcPolicy, Store, StoreOptions};
use protoledger::{Event, Kind, Ledger, LedgerSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::agent;
#[cfg(feature = "sync")]
use super::fixture;

fn ev(i: u64, word: &str) -> Event {
    Event::new(format!("e{i:03}"), Kind::Text).with("role", "user").with("text", format!("{word} {i:03}"))
}

fn after(mut e: Event, a: &str) -> Event {
    e.after = Some(a.to_string());
    e
}

fn truth(s: &Store, id: &str) -> Ledger {
    // A reader made from the path alone carries no cache and reads any form.
    protoledger::store::LedgerReader::plain(s.ledger_path(id).unwrap()).ledger().unwrap()
}

fn uncached(s: &Store, ids: &[&str], format: &str, opts: &ExportOptions) -> formats::Output {
    let mut set = LedgerSet::new();
    for id in ids {
        set.insert(truth(s, id));
    }
    formats::by_name(format).unwrap().export(&set, opts).unwrap().output
}

fn seed(root: &Path, n: u64, compressed: bool, word: &str) {
    let mut s = Store::open(root).unwrap();
    if compressed {
        s.set_compression(true).unwrap();
        s.set_seal_bytes(256).unwrap();
    }
    s.create_agent(agent("a")).unwrap();
    for i in 1..=n {
        s.append("a", ev(i, word)).unwrap();
    }
}

/// append then read (same handle), plain and compressed.
#[test]
fn appends_then_reads_answer_the_ledger_on_disk() {
    for compressed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), 10, compressed, "text");
        let mut s = Store::open(dir.path()).unwrap();
        let _ = s.reader("a").unwrap().ledger().unwrap();
        let _ = s.event_at("a", 10).unwrap();
        let _ = s.render("a", "codex", &ExportOptions::standard()).unwrap();
        for i in 11..=40 {
            s.append("a", ev(i, "text")).unwrap();
            assert_eq!(s.reader("a").unwrap().ledger().unwrap(), truth(&s, "a"), "compressed={compressed} after append {i}");
            assert_eq!(s.event_at("a", i).unwrap().unwrap().str("text"), Some(format!("text {i:03}").as_str()));
            let r = s.render("a", "codex", &ExportOptions::standard()).unwrap();
            assert_eq!(r.exported.output, uncached(&s, &["a"], "codex", &ExportOptions::standard()), "render stale after append {i}");
        }
    }
}

/// redact / unredact then render and reproject.
#[cfg(feature = "sync")]
#[test]
fn redact_and_unredact_then_reproject_write_the_ledger_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path().join("store")).unwrap();
    let id = s.import("claude-code", &fixture("claude-code/plain-chat/native/1ead304c-a7bc-443a-8455-a1778cc62693.jsonl")).unwrap().created[0].clone();
    let out = dir.path().join("cc");
    let b = protoledger::sync::project(&mut s, &id, "claude-code", &out).unwrap();
    protoledger::sync::reproject(&mut s, &b.binding).unwrap();
    let std = ExportOptions::standard();
    let target = {
        let l = s.reader(&id).unwrap().ledger().unwrap();
        l.projected_view().iter().rev().find(|e| e.kind == Kind::Text).unwrap().id.clone()
    };
    let check = |s: &Store, what: &str| {
        let want = uncached(s, &[&id], "claude-code", &std);
        assert_eq!(s.render(&id, "claude-code", &std).unwrap().exported.output, want, "render stale after {what}");
        for (rel, bytes) in &want.files {
            assert_eq!(&std::fs::read(out.join(rel)).unwrap(), bytes, "projected file {rel} stale after {what}");
        }
    };
    s.redact(&id, std::slice::from_ref(&target), "probe").unwrap();
    protoledger::sync::reproject(&mut s, &b.binding).unwrap();
    check(&s, "redact");
    s.unredact(&id, std::slice::from_ref(&target)).unwrap();
    protoledger::sync::reproject(&mut s, &b.binding).unwrap();
    check(&s, "unredact");
    protoledger::sync::reproject(&mut s, &b.binding).unwrap();
    check(&s, "second reproject");
}

fn archive_and_gc(s: &mut Store) {
    s.append("a", Event::new("arch", Kind::App).with("type", "protoledger/archived")).unwrap();
    s.gc(&GcPolicy { older_than: None, larger_than: None, archived: true, remove_projections: false }, std::time::SystemTime::now()).unwrap();
    assert!(s.agent("a").unwrap().is_none(), "sentinel: gc removed a");
}

/// GC then re-create the same id with other events, same handle.
#[test]
fn gc_and_a_ledger_made_again_by_the_same_handle_is_read_from_disk() {
    for compressed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), 8, compressed, "text");
        let mut s = Store::open(dir.path()).unwrap();
        let _ = s.reader("a").unwrap().ledger().unwrap();
        let _ = s.render("a", "codex", &ExportOptions::standard()).unwrap();
        archive_and_gc(&mut s);
        s.create_agent(agent("a")).unwrap();
        for i in 1..=8 {
            s.append("a", ev(i, "TEXT")).unwrap();
        }
        s.append("a", Event::new("arch", Kind::App).with("type", "protoledger/archived")).unwrap();
        assert_eq!(s.event_at("a", 3).unwrap().unwrap().str("text"), Some("TEXT 003"), "compressed={compressed}");
        assert_eq!(s.reader("a").unwrap().ledger().unwrap(), truth(&s, "a"));
        assert_eq!(s.render("a", "codex", &ExportOptions::standard()).unwrap().exported.output, uncached(&s, &["a"], "codex", &ExportOptions::standard()));
    }
}

/// GC then re-create the same id, by ANOTHER store handle on the same
/// directory (what `protoledger store gc` + a re-import does while `serve`
/// holds the store open).
#[test]
fn gc_and_a_ledger_made_again_by_another_handle_is_read_from_disk() {
    for compressed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), 8, compressed, "text");
        let h1 = Store::open(dir.path()).unwrap();
        let _ = h1.reader("a").unwrap().ledger().unwrap();
        let _ = h1.event_at("a", 3).unwrap();
        {
            let mut h2 = Store::open(dir.path()).unwrap();
            archive_and_gc(&mut h2);
            h2.create_agent(agent("a")).unwrap();
            for i in 1..=8 {
                h2.append("a", ev(i, "TEXT")).unwrap();
            }
        }
        let got = h1.event_at("a", 3).unwrap().unwrap();
        let disk = truth(&h1, "a");
        assert_eq!(disk.events[2].str("text"), Some("TEXT 003"), "sentinel: disk holds the new ledger");
        assert_eq!(got.str("text"), Some("TEXT 003"), "compressed={compressed}: h1.event_at served a stale event");
        assert_eq!(h1.reader("a").unwrap().ledger().unwrap(), disk, "compressed={compressed}: h1 reader stale");
    }
}

/// The same, after h1 refreshes its index.
#[test]
fn gc_and_a_ledger_made_again_by_another_handle_is_read_from_disk_after_refresh() {
    for compressed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), 8, compressed, "text");
        let h1 = Store::open(dir.path()).unwrap();
        let _ = h1.reader("a").unwrap().ledger().unwrap();
        {
            let mut h2 = Store::open(dir.path()).unwrap();
            archive_and_gc(&mut h2);
            h2.create_agent(agent("a")).unwrap();
            for i in 1..=8 {
                h2.append("a", ev(i, "TEXT")).unwrap();
            }
        }
        h1.refresh().unwrap();
        assert_eq!(h1.reader("a").unwrap().ledger().unwrap(), truth(&h1, "a"), "h1 reader stale after refresh");
        let r = h1.render("a", "codex", &ExportOptions::standard()).unwrap();
        assert_eq!(r.exported.output, uncached(&h1, &["a"], "codex", &ExportOptions::standard()), "compressed={compressed}: h1 render stale");
    }
}

/// recompress then read / render.
#[test]
fn recompression_and_migration_answer_the_ledger_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), 30, true, "text");
    let mut s = Store::open(dir.path()).unwrap();
    let _ = s.reader("a").unwrap().ledger().unwrap();
    let _ = s.render("a", "codex", &ExportOptions::standard()).unwrap();
    s.recompress(protoledger::store::DictChoice::None, &[]).unwrap();
    s.append("a", ev(31, "text")).unwrap();
    assert_eq!(s.reader("a").unwrap().ledger().unwrap(), truth(&s, "a"));
    assert_eq!(s.render("a", "codex", &ExportOptions::standard()).unwrap().exported.output, uncached(&s, &["a"], "codex", &ExportOptions::standard()));
    // and decompress again (migration to plain)
    s.set_compression(false).unwrap();
    assert_eq!(s.reader("a").unwrap().ledger().unwrap(), truth(&s, "a"));
}

/// rewrite on disk (another process), file grows (plain form): no index
/// update, no catch-up.
#[test]
fn a_rewrite_that_grows_the_ledger_is_read_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), 6, false, "text");
    let s = Store::open(dir.path()).unwrap();
    let _ = s.reader("a").unwrap().ledger().unwrap();
    let path = s.ledger_path("a").unwrap();
    let mut text = std::fs::read_to_string(&path).unwrap().replace("text 00", "TEXT 00");
    let mut e7 = ev(7, "TEXT");
    e7.seq = 7;
    text.push_str(&(serde_json::to_string(&e7).unwrap() + "\n"));
    std::fs::write(&path, &text).unwrap();
    let disk = truth(&s, "a");
    assert_eq!(disk.events[1].str("text"), Some("TEXT 002"), "sentinel");
    let got = s.reader("a").unwrap().ledger().unwrap();
    assert_eq!(got.events[1].str("text"), Some("TEXT 002"), "a grown rewrite was answered from the cache");
}

/// rewrite on disk, same size (plain form), then refresh then read.
#[test]
fn a_rewrite_of_the_same_size_is_read_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), 6, false, "text");
    let s = Store::open(dir.path()).unwrap();
    let _ = s.reader("a").unwrap().ledger().unwrap();
    let path = s.ledger_path("a").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let text = std::fs::read_to_string(&path).unwrap().replace("text 00", "TEXT 00");
    std::fs::write(&path, &text).unwrap();
    assert_eq!(s.reader("a").unwrap().ledger().unwrap(), truth(&s, "a"), "before refresh");
    s.refresh().unwrap();
    assert_eq!(s.reader("a").unwrap().ledger().unwrap(), truth(&s, "a"), "after refresh");
    assert_eq!(s.event_at("a", 2).unwrap().unwrap().str("text"), Some("TEXT 002"));
}

/// two handles: one appends, the other reads.
#[test]
fn one_handle_appends_and_another_reads_the_ledger_on_disk() {
    for compressed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), 5, compressed, "text");
        let reader_h = Store::open(dir.path()).unwrap();
        let mut writer_h = Store::open(dir.path()).unwrap();
        let _ = reader_h.reader("a").unwrap().ledger().unwrap();
        let _ = reader_h.render("a", "codex", &ExportOptions::standard()).unwrap();
        for i in 6..=40 {
            writer_h.append("a", ev(i, "text")).unwrap();
            assert_eq!(reader_h.reader("a").unwrap().ledger().unwrap(), truth(&reader_h, "a"), "compressed={compressed} {i}");
            assert_eq!(reader_h.event_at("a", i).unwrap().map(|e| e.id), Some(format!("e{i:03}")));
            assert_eq!(
                reader_h.render("a", "codex", &ExportOptions::standard()).unwrap().exported.output,
                uncached(&reader_h, &["a"], "codex", &ExportOptions::standard()),
                "compressed={compressed} render {i}"
            );
        }
    }
}

/// N=4 concurrent readers while the store appends (compressed: seals
/// happen under them). Every read is a contiguous prefix with the right text.
#[test]
fn concurrent_readers_under_an_appender_read_contiguous_prefixes() {
    for compressed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), 5, compressed, "text");
        let mut s = Store::open_with(dir.path(), StoreOptions { cache_bytes: Some(64 * 1024) }).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let threads: Vec<_> = (0..4)
            .map(|t| {
                let r = s.reader("a").unwrap();
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut n = 0u64;
                    let mut max = 0;
                    while !stop.load(Ordering::Relaxed) {
                        let l = if t % 2 == 0 { r.ledger().unwrap().events } else { r.tail(0).unwrap().0.into_iter().map(|(_, e)| e).collect() };
                        assert!(l.len() >= max, "thread {t}: went backwards");
                        max = l.len();
                        for (k, e) in l.iter().enumerate() {
                            let i = k as u64 + 1;
                            assert_eq!(e.seq, i, "thread {t}");
                            assert_eq!(e.str("text"), Some(format!("text {i:03}").as_str()), "thread {t}");
                        }
                        n += 1;
                    }
                    n
                })
            })
            .collect();
        for i in 6..=300 {
            s.append("a", ev(i, "text")).unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        for t in threads {
            assert!(t.join().unwrap() > 0);
        }
        assert_eq!(s.reader("a").unwrap().ledger().unwrap(), truth(&s, "a"));
        let st = s.cache_stats();
        assert!(st.bytes <= st.budget && st.events.hits > 0, "{st:?}");
    }
}

/// a view-neutral fact, then a view-changing event, then a redaction;
/// on a branched ledger too.
#[test]
fn a_view_neutral_fact_then_a_view_change_renders_the_ledger_on_disk() {
    for branched in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), 6, false, "text");
        let mut s = Store::open(dir.path()).unwrap();
        if branched {
            s.append("a", after(ev(7, "text"), "e003")).unwrap();
        }
        let std = ExportOptions::standard();
        for f in ["codex", "claude-code"] {
            s.render("a", f, &std).unwrap();
        }
        s.append("a", Event::new("fact-1", Kind::App).with("type", "protoledger/projected").with("data", serde_json::json!({}))).unwrap();
        for f in ["codex", "claude-code"] {
            assert_eq!(s.render("a", f, &std).unwrap().exported.output, uncached(&s, &["a"], f, &std), "{f} after fact, branched={branched}");
        }
        s.append("a", ev(20, "text")).unwrap();
        for f in ["codex", "claude-code"] {
            let r = s.render("a", f, &std).unwrap();
            assert_eq!(r.exported.output, uncached(&s, &["a"], f, &std), "{f} after view change, branched={branched}");
            assert_eq!(r.through.as_deref(), Some("e020"));
        }
        s.append("a", Event::new("fact-2", Kind::App).with("type", "protoledger/projected").with("data", serde_json::json!({}))).unwrap();
        s.append("a", after(ev(21, "text"), "e002")).unwrap();
        for f in ["codex", "claude-code"] {
            assert_eq!(s.render("a", f, &std).unwrap().exported.output, uncached(&s, &["a"], f, &std), "{f} after fact+branch");
        }
        s.append("a", Event::new("fact-3", Kind::App).with("type", "protoledger/projected").with("data", serde_json::json!({}))).unwrap();
        s.redact("a", &["e021".to_string()], "probe").unwrap();
        for f in ["codex", "claude-code"] {
            assert_eq!(s.render("a", f, &std).unwrap().exported.output, uncached(&s, &["a"], f, &std), "{f} after fact+redact");
        }
    }
}

/// Control: the same sequence with the cache off answers right.
#[test]
fn control_with_no_cache_a_ledger_made_again_by_another_handle_is_read() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), 8, false, "text");
    let h1 = Store::open_with(dir.path(), StoreOptions { cache_bytes: Some(0) }).unwrap();
    let _ = h1.reader("a").unwrap().ledger().unwrap();
    let _ = h1.event_at("a", 3).unwrap();
    {
        let mut h2 = Store::open(dir.path()).unwrap();
        archive_and_gc(&mut h2);
        h2.create_agent(agent("a")).unwrap();
        for i in 1..=8 {
            h2.append("a", ev(i, "TEXT")).unwrap();
        }
    }
    assert_eq!(h1.event_at("a", 3).unwrap().unwrap().str("text"), Some("TEXT 003"));
    assert_eq!(h1.reader("a").unwrap().ledger().unwrap(), truth(&h1, "a"));
}

/// Control: cache off answers the grown rewrite from disk.
#[test]
fn control_with_no_cache_a_grown_rewrite_is_read() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), 6, false, "text");
    let s = Store::open_with(dir.path(), StoreOptions { cache_bytes: Some(0) }).unwrap();
    let _ = s.reader("a").unwrap().ledger().unwrap();
    let path = s.ledger_path("a").unwrap();
    let mut text = std::fs::read_to_string(&path).unwrap().replace("text 00", "TEXT 00");
    let mut e7 = ev(7, "TEXT");
    e7.seq = 7;
    text.push_str(&(serde_json::to_string(&e7).unwrap() + "\n"));
    std::fs::write(&path, &text).unwrap();
    assert_eq!(s.reader("a").unwrap().ledger().unwrap().events[1].str("text"), Some("TEXT 002"));
}

/// Plain and compressed, and the RENDER hit: the recreated ledger
/// has the same last seq and length, so h1's render key is unchanged.
#[test]
fn gc_and_a_ledger_made_again_by_another_handle_is_rendered_afresh() {
    for compressed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), 8, compressed, "text");
        let h1 = Store::open(dir.path()).unwrap();
        let before = h1.render("a", "codex", &ExportOptions::standard()).unwrap();
        {
            let mut h2 = Store::open(dir.path()).unwrap();
            archive_and_gc(&mut h2);
            h2.create_agent(agent("a")).unwrap();
            for i in 1..=8 {
                h2.append("a", ev(i, "TEXT")).unwrap();
            }
        }
        let after = h1.render("a", "codex", &ExportOptions::standard()).unwrap();
        let want = uncached(&h1, &["a"], "codex", &ExportOptions::standard());
        assert_ne!(before.exported.output, want, "sentinel: the truth changed");
        let stale_render = after.exported.output != want;
        let stale_event = h1.event_at("a", 3).unwrap().unwrap().str("text") != Some("TEXT 003");
        assert!(!Arc::ptr_eq(&before, &after), "compressed={compressed}: the render before the ledger was made again was answered");
        assert!(!stale_render && !stale_event, "compressed={compressed}: stale_render={stale_render} stale_event={stale_event}");
    }
}

/// A ledger garbage-collected and made again under the same id by ANOTHER
/// PROCESS (`protoledger store gc`, then `protoledger store import`) while
/// this store stays open, as `serve` does: the open store reads, renders
/// and answers `event_at` from the new ledger. Plain and compressed.
#[cfg(feature = "cli")]
#[test]
fn gc_and_a_ledger_made_again_by_another_process_is_read_from_disk() {
    use std::process::Command;
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_protoledger")).args(args).output().unwrap();
        assert!(out.status.success(), "protoledger {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    for compressed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        seed(&store, 8, compressed, "text");
        {
            let mut s = Store::open(&store).unwrap();
            s.append("a", Event::new("arch", Kind::App).with("type", "protoledger/archived")).unwrap();
        }
        // The ledger the other process imports: the same id, the same
        // lengths, other text.
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        {
            let mut ap = protoledger::Appender::open(&src.join("a.jsonl"), Some(agent("a"))).unwrap();
            for i in 1..=8 {
                ap.append(ev(i, "TEXT")).unwrap();
            }
            ap.append(Event::new("arch", Kind::App).with("type", "protoledger/archived")).unwrap();
        }
        let h1 = Store::open(&store).unwrap();
        let std = ExportOptions::standard();
        let before = h1.render("a", "codex", &std).unwrap();
        assert_eq!(h1.event_at("a", 3).unwrap().unwrap().str("text"), Some("text 003"));
        let st = store.to_str().unwrap();
        run(&["store", "gc", st, "--archived", "--yes"]);
        run(&["store", "import", st, "protoledger", src.to_str().unwrap()]);
        assert_eq!(truth(&h1, "a").events[2].str("text"), Some("TEXT 003"), "sentinel: the other process made it again");
        assert_eq!(h1.event_at("a", 3).unwrap().unwrap().str("text"), Some("TEXT 003"), "compressed={compressed}: event_at");
        assert_eq!(h1.reader("a").unwrap().ledger().unwrap(), truth(&h1, "a"), "compressed={compressed}: reader");
        let after = h1.render("a", "codex", &std).unwrap();
        assert!(!Arc::ptr_eq(&before, &after), "compressed={compressed}: the old render was answered");
        assert_eq!(after.exported.output, uncached(&h1, &["a"], "codex", &std), "compressed={compressed}: render");
    }
}

/// THE STATED LIMIT (SPEC §9.6). A plain ledger rewritten in place by a
/// foreign process (a violation of §1.2) so that it grows, with every byte
/// near its old end unchanged, is indistinguishable from an append by
/// anything short of reading the bytes the cache holds: the file is the same
/// file (device, inode, birth time), it is longer, and its modification time
/// moved, exactly as an append leaves it. The cache checks the bytes just
/// before the old end (a rewrite there, as in
/// `a_rewrite_that_grows_the_ledger_is_read_from_disk`, is caught) and no
/// others, so a line far from the end, rewritten, is still answered from
/// the cache. This test pins that limit: if it fails, the cache has learnt
/// to see such a rewrite, and SPEC §9.6 should say so.
#[test]
fn a_growing_rewrite_away_from_the_old_end_is_the_stated_limit() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), 20, false, "text");
    let s = Store::open(dir.path()).unwrap();
    assert_eq!(s.event_at("a", 2).unwrap().unwrap().str("text"), Some("text 002"));
    let path = s.ledger_path("a").unwrap();
    let mut text = std::fs::read_to_string(&path).unwrap().replacen("text 002", "TEXT 002", 1);
    let mut e21 = ev(21, "text");
    e21.seq = 21;
    text.push_str(&(serde_json::to_string(&e21).unwrap() + "\n"));
    std::fs::write(&path, &text).unwrap();
    assert_eq!(truth(&s, "a").events[1].str("text"), Some("TEXT 002"), "sentinel: the disk holds the rewrite");
    assert_eq!(s.event_at("a", 2).unwrap().unwrap().str("text"), Some("text 002"), "the limit moved: update SPEC §9.6");
    // Anything that changes the file's identity, or shrinks it, or rewrites
    // it at its old size, or changes the bytes before its old end, is read
    // from disk (the tests above); a catch-up that re-reads the ledger whole
    // finds the rewrite too and drops the entries.
}
