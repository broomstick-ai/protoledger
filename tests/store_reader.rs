//! The store's ledger reader (SPEC §10.1, §10.5): every read a store makes of
//! a ledger goes through it, and for a plain ledger it reads exactly the
//! bytes the file holds, up to its last complete line.
#![cfg(feature = "store")]

use protoledger::store::{LedgerReader, Store};
use protoledger::{Agent, Event, Kind};
use std::path::Path;

fn agent(id: &str) -> Agent {
    Agent { id: id.into(), created_at: None, source: None, parent: None, links: vec![], meta: Default::default() }
}

fn text(id: &str, t: &str) -> Event {
    Event::new(id, Kind::Text).with("role", "user").with("text", t)
}

/// A store whose agent `a` holds a short line, a non-ASCII line and a line
/// longer than the reader's 64 KiB scan buffer, written by the store itself.
fn seeded(root: &Path) -> Store {
    let mut s = Store::open(root).unwrap();
    s.create_agent(agent("a")).unwrap();
    s.append("a", text("a1", "one")).unwrap();
    s.append("a", text("a2", "naïve café — 日本語 🦀")).unwrap();
    s.append("a", text("a3", &"x".repeat(200_000))).unwrap();
    s.append("a", text("a4", "four")).unwrap();
    s
}

#[test]
fn the_plain_reader_reads_the_file_the_store_wrote_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let s = seeded(dir.path());
    let file = std::fs::read(dir.path().join("ledgers/a.jsonl")).unwrap();
    assert!(file.len() > 200_000 && file.ends_with(b"\n"));
    let r = s.reader("a").unwrap();
    assert_eq!(r, LedgerReader::plain(dir.path().join("ledgers/a.jsonl")));
    assert_eq!(r.len().unwrap(), file.len() as u64);
    assert_eq!(r.stream().unwrap(), file);
    let mut out = Vec::new();
    assert_eq!(r.copy_to(&mut out).unwrap(), file.len() as u64);
    assert_eq!(out, file);
    assert_eq!(r.header().unwrap().unwrap().agent.id, "a");

    // Every indexed offset reads its own event, and the whole ledger parses.
    let rows = s.events("a", 1, usize::MAX).unwrap();
    assert_eq!(rows.len(), 4);
    for row in &rows {
        let e = r.read_at(row.offset).unwrap();
        assert_eq!((e.seq, e.id.as_str()), (row.seq, row.id.as_str()));
    }
    assert_eq!(r.read_at(rows[1].offset).unwrap().str("text"), Some("naïve café — 日本語 🦀"));
    let held = r.ledger().unwrap();
    assert_eq!(held.events.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["a1", "a2", "a3", "a4"]);

    // Tailing from any event's offset yields it and everything after it.
    let (all, end) = r.tail(0).unwrap();
    assert_eq!(all.iter().map(|(o, _)| *o).collect::<Vec<_>>(), rows.iter().map(|r| r.offset).collect::<Vec<_>>());
    assert_eq!(end, file.len() as u64);
    let (rest, end) = r.tail(rows[2].offset).unwrap();
    assert_eq!(rest.iter().map(|(_, e)| e.seq).collect::<Vec<_>>(), [3, 4]);
    assert_eq!(end, file.len() as u64);
}

#[test]
fn a_torn_final_line_is_not_part_of_the_stream() {
    let dir = tempfile::tempdir().unwrap();
    let ledgers = dir.path().join("ledgers");
    std::fs::create_dir_all(&ledgers).unwrap();
    let complete =
        "{\"protoledger\":\"0.1\",\"agent\":{\"id\":\"t\"}}\n{\"id\":\"t1\",\"seq\":1,\"kind\":\"text\",\"body\":{\"role\":\"user\",\"text\":\"done\"}}\n";
    // Longer than the reader's 64 KiB scan buffer, so finding the last
    // complete line takes more than one backwards read.
    let torn = format!("{{\"id\":\"t2\",\"seq\":2,\"kind\":\"text\",\"body\":{{\"text\":\"{}", "z".repeat(150_000));
    std::fs::write(ledgers.join("t.jsonl"), format!("{complete}{torn}")).unwrap();
    // Opened without a writer: nothing repairs the torn tail.
    let s = Store::open(dir.path()).unwrap();
    let r = s.reader("t").unwrap();
    assert_eq!(std::fs::metadata(ledgers.join("t.jsonl")).unwrap().len(), (complete.len() + torn.len()) as u64);
    assert_eq!(r.len().unwrap(), complete.len() as u64);
    assert_eq!(r.stream().unwrap(), complete.as_bytes());
    assert_eq!(r.ledger().unwrap().events.len(), 1);
    let (evs, end) = r.tail(0).unwrap();
    assert_eq!((evs.len(), end), (1, complete.len() as u64));
}

#[test]
fn a_stream_with_no_complete_line_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.jsonl");
    std::fs::write(&p, "{\"protoledger\":").unwrap();
    let r = LedgerReader::plain(&p);
    assert!(r.is_empty().unwrap());
    assert!(r.stream().unwrap().is_empty());
    assert!(r.header().unwrap().is_none());
}

#[test]
fn the_reader_of_an_agent_the_store_does_not_hold_is_an_error_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    let s = seeded(dir.path());
    let err = s.reader("nobody").unwrap_err().to_string();
    assert!(err.contains("\"nobody\""), "{err}");
}
