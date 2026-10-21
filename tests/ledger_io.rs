//! Appender, tail and read_at, and unknown kinds (SPEC §2).

use protoledger::{Agent, Appender, Event, Kind, Ledger, read_at, tail};
use std::io::Write;

fn agent(id: &str) -> Agent {
    Agent { id: id.into(), created_at: None, source: None, parent: None, links: vec![], meta: Default::default() }
}

#[test]
fn append_returns_offsets_that_tail_and_read_at_agree_with() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.jsonl");
    let mut ap = Appender::open(&path, Some(agent("a"))).unwrap();
    let header_len = ap.len();
    let first = ap.append(Event::new("e1", Kind::Text).with("role", "user").with("text", "hi")).unwrap();
    let second = ap.append(Event::new("e2", Kind::Text).with("role", "assistant").with("text", "hello")).unwrap();
    assert_eq!(first.offset, header_len);
    assert_eq!(second.offset, first.offset + first.len);
    assert_eq!(read_at(&path, second.offset).unwrap().id, "e2");
    let (all, end) = tail(&path, 0).unwrap();
    assert_eq!(all.iter().map(|(o, e)| (*o, e.id.as_str())).collect::<Vec<_>>(), vec![(first.offset, "e1"), (second.offset, "e2")]);
    assert_eq!(end, ap.len());
    let (rest, _) = tail(&path, second.offset).unwrap();
    assert_eq!(rest.len(), 1);
    drop(ap);
    // A torn tail is left for the next call, and repaired by the next writer.
    std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"{\"id\":\"e3\"").unwrap();
    let (none, same) = tail(&path, end).unwrap();
    assert!(none.is_empty());
    assert_eq!(same, end);
    let mut ap = Appender::open(&path, None).unwrap();
    assert_eq!(ap.append(Event::new("e3", Kind::Title).with("text", "t")).unwrap().offset, end);
}

#[test]
fn a_second_writer_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.jsonl");
    let _first = Appender::open(&path, Some(agent("a"))).unwrap();
    assert!(Appender::open(&path, None).is_err());
}

#[test]
fn unknown_kinds_are_read_as_native_and_written_back_by_name() {
    let text = "{\"protoledger\":\"0.1\",\"agent\":{\"id\":\"a\"}}\n{\"id\":\"e1\",\"seq\":1,\"kind\":\"hologram\",\"body\":{\"x\":1}}\n";
    let (l, _) = Ledger::from_jsonl(text).unwrap();
    assert_eq!(l.events[0].kind, Kind::Native);
    assert_eq!(l.events[0].unknown_kind.as_deref(), Some("hologram"));
    assert_eq!(l.to_jsonl(), text);
}

#[test]
fn shared_index_files_are_appended_not_replaced() {
    use protoledger::formats::Output;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("session_index.jsonl"), "{\"id\":\"old\"}\n").unwrap();
    let mut out = Output::default();
    out.put_append("session_index.jsonl", "{\"id\":\"old\"}\n{\"id\":\"new\"}\n");
    out.write_to(dir.path(), true).unwrap();
    assert_eq!(std::fs::read_to_string(dir.path().join("session_index.jsonl")).unwrap(), "{\"id\":\"old\"}\n{\"id\":\"new\"}\n");
}
