//! The read cache (SPEC §9.6): a line read again is a hit that touches no
//! file and no decompressor; the byte budget holds under concurrent readers
//! and evicts the least recently used first; an entry over the budget is not
//! cached; a thread re-rendered into the same harness unchanged is a hit and
//! any change misses; GC, a rewrite and damage drop entries; recompression
//! keeps them; a budget of 0 reads exactly as without a cache.
//!
//! What touched the disk is counted at the reader seam
//! (`protoledger::store::testing::counts`, per thread).

use protoledger::formats::{self, ExportOptions};
use protoledger::store::testing::counts;
use protoledger::store::{CacheStats, DictChoice, GcPolicy, Store, StoreOptions};
use protoledger::{Event, Kind, LedgerSet};
use std::path::Path;
use std::sync::Arc;

use super::{agent, fixture};

/// Events whose lines all have the same length (fixed-width ids and texts).
fn even(i: u64) -> Event {
    Event::new(format!("e{i:03}"), Kind::Text).with("role", "user").with("text", format!("text {i:03}"))
}

/// A store at `root` with agent `a` holding `n` equal-length events, then
/// reopened (a fresh cache) with `opts`.
fn even_store(root: &Path, n: u64, compressed: bool, opts: StoreOptions) -> Store {
    {
        let mut s = Store::open(root).unwrap();
        if compressed {
            s.set_compression(true).unwrap();
            s.set_seal_bytes(256).unwrap();
        }
        s.create_agent(agent("a")).unwrap();
        for i in 1..=n {
            s.append("a", even(i)).unwrap();
        }
    }
    Store::open_with(root, opts).unwrap()
}

/// The disk reads and decompressions `f` made on this thread.
fn touched<T>(f: impl FnOnce() -> T) -> (T, u64, u64) {
    let before = counts();
    let t = f();
    let after = counts();
    (t, after.reads - before.reads, after.decompressions - before.decompressions)
}

/// (a) In a compressed ledger sealed into many blocks: the first read of an
/// event decompresses its block; the second is a hit and touches neither a
/// file nor the decompressor. So does a second whole-ledger read.
#[test]
fn a_second_read_of_an_event_is_a_hit_and_touches_no_decompressor() {
    let dir = tempfile::tempdir().unwrap();
    let s = even_store(dir.path(), 40, true, StoreOptions::default());
    assert!(s.blocks("a").unwrap().len() > 3, "sentinel: the ledger is in several blocks");
    let (first, reads, dec) = touched(|| s.event_at("a", 7).unwrap().unwrap());
    assert!(reads >= 1 && dec >= 1, "sentinel: the first read decompressed ({reads} reads, {dec} decompressions)");
    let (second, reads, dec) = touched(|| s.event_at("a", 7).unwrap().unwrap());
    assert_eq!(second, first);
    assert_eq!((reads, dec), (0, 0), "the second read of seq 7 went to disk");
    assert_eq!(s.cache_stats().events.hits, 1);

    let (l1, _, dec1) = touched(|| s.reader("a").unwrap().ledger().unwrap());
    assert!(dec1 > 0);
    let (l2, reads, dec) = touched(|| s.reader("a").unwrap().ledger().unwrap());
    assert_eq!(l2, l1);
    assert_eq!((reads, dec), (0, 0), "the second whole read went to disk");
    // And a tail (a subscriber's backlog) from the middle on.
    let (t, reads, dec) = touched(|| s.reader("a").unwrap().tail(s.events("a", 20, 1).unwrap()[0].offset).unwrap());
    assert_eq!(t.0.len(), 21);
    assert_eq!((reads, dec), (0, 0));
}

/// (b) Single-threaded first, where the order is exact: three lines fit;
/// reading 1, 2, 3, then 1 again, then 4 evicts 2 (the least recently used),
/// not 1 (the first in, which a FIFO would evict). Then N = 4 threads read
/// through clones of the store's readers at once, and the budget holds after
/// every read on every thread.
#[test]
fn the_budget_holds_under_concurrent_readers_and_evicts_least_recently_used_first() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = even_store(dir.path(), 9, true, StoreOptions::default());
    s.event_at("a", 9).unwrap();
    let w = s.cache_stats().events.bytes;
    assert!(w > 0);
    s.set_cache_bytes(3 * w + w / 2);
    assert_eq!(s.cache_stats().entries, 1);
    s.set_cache_bytes(0);
    s.set_cache_bytes(3 * w + w / 2);
    let before = s.cache_stats().events.evictions;
    for seq in [1, 2, 3, 1, 4] {
        s.event_at("a", seq).unwrap();
    }
    let st = s.cache_stats();
    assert_eq!((st.entries, st.events.evictions - before), (3, 1), "{st:?}");
    let (_, reads, _) = touched(|| s.event_at("a", 1).unwrap());
    assert_eq!(reads, 0, "1 was evicted: the cache evicted first in, not least recently used");
    let (_, reads, _) = touched(|| s.event_at("a", 2).unwrap());
    assert!(reads > 0, "2 is still cached: nothing was evicted for 4");

    // N = 4 concurrent readers, each in its own order, budget 5 lines.
    let budget = 5 * w;
    s.set_cache_bytes(budget);
    let reader = s.reader("a").unwrap();
    let rows = s.events("a", 1, usize::MAX).unwrap();
    let want: Vec<Event> = rows.iter().map(|r| s.event_at("a", r.seq).unwrap().unwrap()).collect();
    let rows = Arc::new(rows);
    let want = Arc::new(want);
    let threads: Vec<_> = (0..4u64)
        .map(|t| {
            let (r, rows, want) = (reader.clone(), rows.clone(), want.clone());
            std::thread::spawn(move || {
                let mut max = 0;
                for round in 0..200u64 {
                    let i = ((round * (t + 1) * 7 + t) % rows.len() as u64) as usize;
                    let e = r.read_seq_at(rows[i].offset, rows[i].seq).unwrap();
                    assert_eq!(e, want[i], "thread {t} read seq {} wrong", rows[i].seq);
                    let st: CacheStats = r.cache_stats().unwrap();
                    assert!(st.bytes <= st.budget, "thread {t}: {} bytes held over a budget of {}", st.bytes, st.budget);
                    max = max.max(st.bytes);
                }
                max
            })
        })
        .collect();
    for t in threads {
        assert!(t.join().unwrap() <= budget);
    }
    let st = s.cache_stats();
    assert!(st.bytes <= budget && st.events.evictions > 1, "{st:?}");
}

/// (c) An entry heavier than the whole budget is not cached, and evicts nothing.
#[test]
fn an_entry_over_the_budget_is_not_cached() {
    let dir = tempfile::tempdir().unwrap();
    let s = even_store(dir.path(), 3, false, StoreOptions { cache_bytes: Some(100) });
    for _ in 0..2 {
        let (_, reads, _) = touched(|| s.event_at("a", 2).unwrap());
        assert!(reads > 0, "a line weighing more than 100 bytes was cached");
    }
    let st = s.cache_stats();
    assert_eq!((st.entries, st.bytes, st.events.oversized, st.events.evictions), (0, 0, 2, 0), "{st:?}");
}

/// The export of `agent`'s thread as `format`, made without the store's
/// cache: from the ledger files, through the export entry point.
fn uncached_render(s: &Store, ids: &[&str], format: &str, opts: &ExportOptions) -> formats::Output {
    let mut set = LedgerSet::new();
    for id in ids {
        set.insert(protoledger::Ledger::read(&s.ledger_path(id).unwrap()).unwrap());
    }
    formats::by_name(format).unwrap().export(&set, opts).unwrap().output
}

/// (d) Rendering an unchanged thread into the same harness again is a hit
/// that reads nothing; appending one event, or redacting one, misses, and
/// the new render is what an uncached export makes; another harness, or
/// another carry setting, is another entry.
#[test]
fn rerendering_an_unchanged_thread_is_a_hit_and_any_change_misses() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    let id = {
        let mut s = Store::open(&root).unwrap();
        s.import("claude-code", &fixture("claude-code/plain-chat/native/1ead304c-a7bc-443a-8455-a1778cc62693.jsonl")).unwrap().created[0].clone()
    };
    let mut s = Store::open(&root).unwrap();
    let std = ExportOptions::standard();
    let r1 = s.render(&id, "claude-code", &std).unwrap();
    assert_eq!(r1.exported.output, uncached_render(&s, &[&id], "claude-code", &std));
    let (r2, reads, dec) = touched(|| s.render(&id, "claude-code", &std).unwrap());
    assert!(Arc::ptr_eq(&r1, &r2), "the unchanged thread was rendered again");
    assert_eq!((reads, dec), (0, 0));
    assert_eq!(s.cache_stats().projections.hits, 1);

    // Another harness, another carry: other entries.
    let codex = s.render(&id, "codex", &std).unwrap();
    assert_eq!(codex.exported.output, uncached_render(&s, &[&id], "codex", &std));
    let bare = s.render(&id, "claude-code", &ExportOptions::no_carry()).unwrap();
    assert!(!Arc::ptr_eq(&bare, &r1));
    assert_eq!(bare.exported.output, uncached_render(&s, &[&id], "claude-code", &ExportOptions::no_carry()));
    assert_eq!(s.cache_stats().projections.hits, 1, "another harness or carry setting was a hit");

    // One event appended: a miss, and the new render holds it.
    s.append(&id, Event::new("appended-1", Kind::Text).with("role", "user").with("text", "one more turn")).unwrap();
    let r3 = s.render(&id, "claude-code", &std).unwrap();
    assert!(!Arc::ptr_eq(&r1, &r3));
    assert_eq!(r3.exported.output, uncached_render(&s, &[&id], "claude-code", &std));
    assert!(String::from_utf8_lossy(r3.exported.output.files.values().next().unwrap()).contains("one more turn"));
    assert_eq!(r3.through.as_deref(), Some("appended-1"));

    // One event redacted: a miss, and the new render leaves it out.
    s.redact(&id, &["appended-1".to_string()], "test").unwrap();
    let r4 = s.render(&id, "claude-code", &std).unwrap();
    assert_eq!(r4.exported.output, uncached_render(&s, &[&id], "claude-code", &std));
    assert!(!String::from_utf8_lossy(r4.exported.output.files.values().next().unwrap()).contains("one more turn"));
    let hits = s.cache_stats().projections.hits;
    assert_eq!(hits, 1, "a changed view was a hit");
    let r5 = s.render(&id, "claude-code", &std).unwrap();
    assert!(Arc::ptr_eq(&r4, &r5));

    // A §11 fact no view holds (a `protoledger/projected`) leaves the
    // render as it was: still a hit, and still what an uncached export
    // makes. Not for the `protoledger` format, whose export keeps facts.
    let own = s.render(&id, "protoledger", &std).unwrap();
    s.append(&id, Event::new("fact-1", Kind::App).with("type", "protoledger/projected").with("data", serde_json::json!({}))).unwrap();
    let r6 = s.render(&id, "claude-code", &std).unwrap();
    assert!(Arc::ptr_eq(&r4, &r6), "a fact no view holds made the render miss");
    assert_eq!(r6.exported.output, uncached_render(&s, &[&id], "claude-code", &std));
    let own2 = s.render(&id, "protoledger", &std).unwrap();
    assert!(!Arc::ptr_eq(&own, &own2), "the protoledger format's render was carried past a fact it holds");
    assert_eq!(own2.exported.output, uncached_render(&s, &[&id], "protoledger", &std));
}

/// (d) Through `reproject`: the second re-projection of an unchanged thread
/// renders nothing (a projection hit) and writes nothing.
#[cfg(feature = "sync")]
#[test]
fn reprojecting_an_unchanged_thread_twice_is_a_projection_hit() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path().join("store")).unwrap();
    let id = s.import("claude-code", &fixture("claude-code/plain-chat/native/1ead304c-a7bc-443a-8455-a1778cc62693.jsonl")).unwrap().created[0].clone();
    let b = protoledger::sync::project(&mut s, &id, "claude-code", &dir.path().join("cc")).unwrap();
    let first = protoledger::sync::reproject(&mut s, &b.binding).unwrap();
    assert!(first.fact.is_some(), "sentinel: the first re-projection appends a protoledger/projected fact");
    let before = s.cache_stats().projections;
    let again = protoledger::sync::reproject(&mut s, &b.binding).unwrap();
    assert!(again.written.is_empty() && again.fact.is_none());
    let after = s.cache_stats().projections;
    assert_eq!((after.hits - before.hits, after.misses - before.misses), (1, 0), "{before:?} → {after:?}");
}

/// (e) GC removing a ledger drops its lines and every render of it.
#[test]
fn gc_deleting_a_ledger_drops_its_entries() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = even_store(dir.path(), 5, false, StoreOptions::default());
    s.create_agent(agent("b")).unwrap();
    s.append("b", even(1)).unwrap();
    s.append("a", Event::new("arch", Kind::App).with("type", "protoledger/archived")).unwrap();
    s.reader("a").unwrap().ledger().unwrap();
    s.reader("b").unwrap().ledger().unwrap();
    s.render("a", "protoledger", &ExportOptions::standard()).unwrap();
    let held = s.cache_stats();
    let plan = s.gc(&GcPolicy { older_than: None, larger_than: None, archived: true, remove_projections: false }, std::time::SystemTime::now()).unwrap();
    assert_eq!(plan.removals.iter().map(|r| r.agent.id.as_str()).collect::<Vec<_>>(), ["a"]);
    let st = s.cache_stats();
    // a's header and 6 events, and its render, are gone; b's 2 lines stay.
    assert_eq!((st.events.entries, st.projections.entries), (2, 0), "{held:?} → {st:?}");
    assert_eq!((st.events.dropped, st.projections.dropped), (7, 1));
}

/// (f) Recompression keeps every line entry, byte for byte (the catch-up
/// after it re-reads the ledger whole and compares each entry's bytes with
/// the line at its offset), and they still answer without a decompression.
/// Opposed: a ledger made again under the same id, with other bytes at the
/// same offsets, drops them.
#[test]
fn recompression_keeps_line_entries_and_a_ledger_made_again_drops_them() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = even_store(dir.path(), 30, true, StoreOptions::default());
    let l1 = s.reader("a").unwrap().ledger().unwrap();
    let held = s.cache_stats();
    let gen0 = s.blocks("a").unwrap();
    let r = s.recompress(DictChoice::None, &[]).unwrap();
    assert_eq!(r.recompressed, ["a"], "{r:?}");
    assert_ne!(s.blocks("a").unwrap(), gen0, "sentinel: the block file was rewritten");
    let st = s.cache_stats();
    assert_eq!((st.events.entries, st.events.dropped), (held.events.entries, 0), "{held:?} → {st:?}");
    let (l2, _, dec) = touched(|| s.reader("a").unwrap().ledger().unwrap());
    assert_eq!((l2, dec), (l1, 0));

    // The ledger deleted and made again (same id, same lengths, other text),
    // behind the store's back: the next catch-up finds other bytes.
    let path = s.ledger_path("a").unwrap();
    drop(s);
    for ext in ["", ".zst", ".tail"] {
        let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
    }
    {
        let mut ap = protoledger::Appender::open(&path, Some(agent("a"))).unwrap();
        for i in 1..=30 {
            ap.append(Event::new(format!("e{i:03}"), Kind::Text).with("role", "user").with("text", format!("TEXT {i:03}"))).unwrap();
        }
    }
    let s2 = Store::open(dir.path()).unwrap();
    let e = s2.event_at("a", 3).unwrap().unwrap();
    assert_eq!(e.str("text"), Some("TEXT 003"));
    // (A fresh store has a fresh cache; the same check in one process:)
    let mut s3 = even_store(&dir.path().join("again"), 4, false, StoreOptions::default());
    s3.reader("a").unwrap().ledger().unwrap();
    let path = s3.ledger_path("a").unwrap();
    let text = std::fs::read_to_string(&path).unwrap().replace("text 00", "TEXT 00");
    std::fs::write(&path, text).unwrap();
    // Before any catch-up: the file is not the one the cache read, nor grown
    // from it, so the reader drops its entries and reads the disk (and
    // caches what it read there).
    let l = s3.reader("a").unwrap().ledger().unwrap();
    assert_eq!(l.events[1].str("text"), Some("TEXT 002"), "a rewritten ledger was answered from the cache");
    let st = s3.cache_stats();
    assert_eq!((st.events.dropped, st.events.entries), (5, 5), "the old entries went, the new ones came: {st:?}");
    s3.reindex().unwrap();
    assert_eq!(s3.event_at("a", 2).unwrap().unwrap().str("text"), Some("TEXT 002"));
}

/// A catch-up that finds a ledger damaged drops its entries (and every
/// operation on it is refused, naming it).
#[test]
fn a_ledger_found_damaged_drops_its_entries() {
    let dir = tempfile::tempdir().unwrap();
    let s = even_store(dir.path(), 4, false, StoreOptions::default());
    s.reader("a").unwrap().ledger().unwrap();
    assert_eq!(s.cache_stats().events.entries, 5);
    let path = s.ledger_path("a").unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let mut lines: Vec<&str> = text.lines().collect();
    lines[2] = "{this line is not json";
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    s.refresh().unwrap();
    assert!(s.damaged_agent("a").is_some(), "sentinel: the store found it damaged");
    let st = s.cache_stats();
    assert_eq!((st.events.entries, st.events.dropped), (0, 5), "{st:?}");
}

/// (g) A budget of 0: every read goes to disk, every render renders, and
/// the answers are what the cached store gives.
#[test]
fn a_budget_of_zero_reads_exactly_as_before() {
    let dir = tempfile::tempdir().unwrap();
    let off = even_store(&dir.path().join("off"), 20, true, StoreOptions { cache_bytes: Some(0) });
    let on = even_store(&dir.path().join("on"), 20, true, StoreOptions::default());
    for _ in 0..2 {
        let (e, reads, dec) = touched(|| off.event_at("a", 5).unwrap());
        assert!(reads > 0 && dec > 0);
        assert_eq!(e, on.event_at("a", 5).unwrap());
        let (l, reads, _) = touched(|| off.reader("a").unwrap().ledger().unwrap());
        assert!(reads > 0);
        assert_eq!(l, on.reader("a").unwrap().ledger().unwrap());
    }
    let r1 = off.render("a", "codex", &ExportOptions::standard()).unwrap();
    let r2 = off.render("a", "codex", &ExportOptions::standard()).unwrap();
    assert!(!Arc::ptr_eq(&r1, &r2));
    assert_eq!(r1.exported.output, on.render("a", "codex", &ExportOptions::standard()).unwrap().exported.output);
    assert_eq!(off.cache_stats(), CacheStats::default());
    assert!(off.reader("a").unwrap().cache_stats().is_none());
}

/// The measured effect (run with `--ignored --nocapture`): a large fixture
/// in a compressed store, read twice and re-rendered twice, cache on and off.
#[test]
#[ignore]
fn measure_the_effect() {
    let fx = fixture("claude-code/v2.1.11-tool-results/native/4693a7cf-a43a-4568-a19c-9ece7613e294.jsonl");
    for budget in [None, Some(0)] {
        let dir = tempfile::tempdir().unwrap();
        let id = {
            let mut s = Store::open(dir.path()).unwrap();
            s.set_compression(true).unwrap();
            s.set_seal_bytes(16 * 1024).unwrap();
            s.import("claude-code", &fx).unwrap().created[0].clone()
        };
        let s = Store::open_with(dir.path(), StoreOptions { cache_bytes: budget }).unwrap();
        let label = if budget == Some(0) { "off" } else { "on " };
        let len = s.agent(&id).unwrap().unwrap().byte_len;
        for pass in 1..=2 {
            let t = std::time::Instant::now();
            let (_, reads, dec) = touched(|| s.reader(&id).unwrap().ledger().unwrap());
            println!(
                "cache {label} read   pass {pass}: {:>8.2?}  {reads} reads, {dec} decompressions ({len} bytes, {} blocks)",
                t.elapsed(),
                s.blocks(&id).unwrap().len()
            );
        }
        for pass in 1..=2 {
            let t = std::time::Instant::now();
            let (_, reads, dec) = touched(|| s.render(&id, "codex", &ExportOptions::standard()).unwrap());
            println!("cache {label} render pass {pass}: {:>8.2?}  {reads} reads, {dec} decompressions", t.elapsed());
        }
    }
}
