//! A store container (a ledger of native records alone that the set's other
//! ledgers are `member-of`: opencode's per-store ledger, a Slack export's
//! `slack:workspace`) is not rendered as a conversation by an export without
//! carry to another format (SPEC §2.3), and still goes back to its own format.
//!
//! The containers here are found by this file's own rule (every event
//! `native`, named by a `member-of` link), not by the crate's predicate, so a
//! wrong predicate cannot agree with itself.

#![cfg(feature = "sqlite")]

use protoledger::formats::{self, ExportOptions, Format, ImportOptions};
use protoledger::loss::{CompareOptions, compare};
use protoledger::roundtrip::{FileDiff, materialise, read_tree};
use protoledger::{Event, Kind, LedgerSet};
use std::path::{Path, PathBuf};

const LOSS: &str = "container-not-rendered";

fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(rel)
}

fn import(format: &str, input: &Path) -> LedgerSet {
    formats::by_name(format).unwrap().import(input, &ImportOptions::standard()).unwrap_or_else(|e| panic!("{format}: {e:#}")).set
}

/// The ids of `set`'s containers, by this file's rule.
fn containers(set: &LedgerSet) -> Vec<String> {
    set.iter()
        .filter(|l| !l.events.is_empty() && l.events.iter().all(|e| e.kind == Kind::Native))
        .filter(|l| set.iter().any(|o| o.agent().links.iter().any(|k| k.rel == "member-of" && k.agent.as_deref() == Some(l.id()))))
        .map(|l| l.id().to_string())
        .collect()
}

fn without(set: &LedgerSet, ids: &[String]) -> LedgerSet {
    let mut s = set.clone();
    s.ledgers.retain(|id, _| !ids.contains(id));
    s
}

fn reimport(f: &dyn Format, out: &formats::Output) -> LedgerSet {
    let dir = tempfile::tempdir().unwrap();
    let path = materialise(out, dir.path()).unwrap();
    f.import(&path, &ImportOptions { variant: None, restore_carried: false }).unwrap_or_else(|e| panic!("{}: {e:#}", f.name())).set
}

/// `claude-code` writes a ledger with no conversation as an empty session;
/// `atif` cannot hold an empty trajectory and invents a step for one. Each
/// renders the container when handed it directly (the sentinel below), so
/// the entry point is what leaves it out.
const VIAS: [(&str, &dyn Format); 2] = [("claude-code", &formats::claude_code::ClaudeCode), ("atif", &formats::atif::Atif)];

/// Exported without carry via each of [`VIAS`], `set` writes exactly what the
/// set without its container writes (its sessions, as they export alone),
/// re-imports with one agent per session, and names the container's records
/// `container-not-rendered`.
fn container_not_rendered(set: &LedgerSet, id_prefix: &str) {
    let ids = containers(set);
    assert!(ids.len() == 1 && ids[0].starts_with(id_prefix), "the fixture is not the one this test is about: containers {ids:?}");
    let records = set.get(&ids[0]).unwrap().events.len() as u64;
    let sessions = without(set, &ids);
    assert!(!sessions.is_empty(), "the fixture holds no session");
    for (via, inner) in VIAS {
        // Sentinel: the adapter itself renders the container as a conversation.
        let alone = LedgerSet::single(set.get(&ids[0]).unwrap().clone());
        let raw = inner.export(&alone, &ExportOptions::no_carry()).unwrap_or_else(|e| panic!("{via}: {e:#}"));
        assert!(
            !raw.output.files.is_empty() && reimport(inner, &raw.output).len() == 1,
            "{via}: the adapter does not render a container, so this test proves nothing"
        );

        let f = formats::by_name(via).unwrap();
        let ex = f.export(set, &ExportOptions::no_carry()).unwrap_or_else(|e| panic!("{via}: {e:#}"));
        let base = f.export(&sessions, &ExportOptions::no_carry()).unwrap_or_else(|e| panic!("{via}: {e:#}"));
        let extra: Vec<&String> = ex.output.files.keys().filter(|k| !base.output.files.contains_key(*k)).collect();
        assert!(extra.is_empty(), "{via}: the container was rendered as a conversation: {extra:?}");
        assert!(ex.output == base.output, "{via}: the sessions do not export as they do without the container");
        let back = reimport(f, &ex.output);
        assert_eq!(back.len(), sessions.len(), "{via}: {} agents came back for {} sessions", back.len(), sessions.len());
        let named = ex.losses.0.iter().find(|l| l.code == LOSS).map(|l| l.count);
        assert_eq!(named, Some(records), "{via}: the container's {records} records are not named {LOSS}");
    }
}

#[test]
fn opencode_store_ledger_is_not_rendered_as_a_conversation_without_carry() {
    let set = import("opencode", &fixture("opencode/v1-sqlite-1.19.3-generated/native/opencode.db"));
    container_not_rendered(&set, "opencode:store:");
}

#[test]
fn slack_workspace_ledger_is_not_rendered_as_a_conversation_without_carry() {
    let set = import("slack", &fixture("slack/export-dir-workspace/native"));
    container_not_rendered(&set, "slack:workspace");
}

/// Opposing: the same container plus one `text` event is a conversation, and
/// every via renders it, with nothing named `container-not-rendered`.
#[test]
fn a_ledger_with_one_conversational_event_and_native_records_is_rendered() {
    for (format, input) in [("opencode", "opencode/v1-sqlite-1.19.3-generated/native/opencode.db"), ("slack", "slack/export-dir-workspace/native")] {
        let mut set = import(format, &fixture(input));
        let ids = containers(&set);
        assert_eq!(ids.len(), 1, "{format}: containers {ids:?}");
        let said = "One conversational event among the native records.";
        let mut l = set.get(&ids[0]).unwrap().clone();
        l.append(Event::new("one-text", Kind::Text).with("role", "user").with("text", said)).unwrap();
        set.insert(l);
        assert!(containers(&set).is_empty(), "{format}: the ledger still holds native records alone");
        for (via, _) in VIAS {
            let f = formats::by_name(via).unwrap();
            let ex = f.export(&set, &ExportOptions::no_carry()).unwrap_or_else(|e| panic!("{format} via {via}: {e:#}"));
            assert!(ex.losses.0.iter().all(|l| l.code != LOSS), "{format} via {via}: a conversation was left out as a container");
            let back = reimport(f, &ex.output);
            assert_eq!(back.len(), set.len(), "{format} via {via}: {} agents came back for {}", back.len(), set.len());
            let found = back.iter().flat_map(|l| l.events.iter()).any(|e| e.str("text").is_some_and(|t| t.contains(said)));
            assert!(found, "{format} via {via}: the conversational event was not rendered");
        }
    }
}

/// The degraded regime and who is in it: with carry the container IS still
/// rendered (its rendering carries its records back to its own format
/// exactly), so carry-on exports of both containers name nothing.
#[test]
fn with_carry_the_opencode_and_slack_containers_are_still_rendered() {
    for (format, input) in [("opencode", "opencode/v1-sqlite-1.19.3-generated/native/opencode.db"), ("slack", "slack/export-dir-workspace/native")] {
        let set = import(format, &fixture(input));
        let ids = containers(&set);
        assert_eq!(ids.len(), 1, "{format}: containers {ids:?}");
        let f = formats::by_name("claude-code").unwrap();
        let ex = f.export(&set, &ExportOptions::standard()).unwrap();
        assert!(ex.losses.0.iter().all(|l| l.code != LOSS), "{format}: carry-on export left the container out");
        assert_eq!(reimport(f, &ex.output).len(), set.len(), "{format}: carry-on export did not render every ledger");
    }
}

/// Its own format still writes the container's records back: an opencode
/// store re-imports with the store ledger's every row, and a Slack export
/// directory comes back byte-exact, both through the entry point and without
/// carry (the regime that leaves containers out of other formats).
#[test]
fn native_self_trip_of_opencode_and_slack_keeps_the_container() {
    for (format, input) in [("opencode", "opencode/v1-sqlite-1.19.3-generated/native/opencode.db"), ("slack", "slack/export-dir-workspace/native")] {
        let f = formats::by_name(format).unwrap();
        let path = fixture(input);
        let p0 = import(format, &path);
        let ids = containers(&p0);
        assert_eq!(ids.len(), 1, "{format}: containers {ids:?}");
        let ex = f.export(&p0, &ExportOptions::no_carry()).unwrap_or_else(|e| panic!("{format}: {e:#}"));
        assert!(ex.losses.0.iter().all(|l| l.code != LOSS), "{format}: its own format left its container out");
        let dir = tempfile::tempdir().unwrap();
        let back_path = materialise(&ex.output, dir.path()).unwrap();
        let p1 = f.import(&back_path, &ImportOptions::standard()).unwrap().set;
        let d = compare(&p0, &p1, CompareOptions::default());
        assert!(d.is_clean(), "{format}: self trip not clean: {}", d.summary());
        let (a, b) = (p0.get(&ids[0]).unwrap(), p1.get(&ids[0]).unwrap_or_else(|| panic!("{format}: the container did not come back")));
        let rec = |l: &protoledger::Ledger| {
            l.events
                .iter()
                .map(|e| e.native.as_ref().map(|n| (n.id.clone(), n.record.as_ref().map(|r| r.get().to_string()), n.raw.clone())))
                .collect::<Vec<_>>()
        };
        assert!(rec(a) == rec(b), "{format}: the container's native records differ after the trip");
        if format == "slack" {
            let fd = FileDiff::of(&read_tree(&path).unwrap(), &ex.output);
            assert!(fd.is_exact(), "slack: export directory not byte-exact: {fd:?}");
        }
    }
}

/// The rule itself: no event, or one event of a conversational kind, is a
/// conversation; native and app records alone are not.
#[test]
fn holds_no_conversation_is_decided_by_event_kinds() {
    let agent = |id: &str| protoledger::Agent { id: id.into(), created_at: None, source: None, parent: None, links: vec![], meta: Default::default() };
    let ledger = |evs: Vec<Event>| {
        let mut l = protoledger::Ledger::new(agent("x"));
        for e in evs {
            l.append(e).unwrap();
        }
        l
    };
    let native = |id: &str| Event::new(id, Kind::Native);
    let app = |id: &str| Event::new(id, Kind::App).with("type", "example/app");
    assert!(!formats::holds_no_conversation(&ledger(vec![])), "an empty ledger");
    assert!(formats::holds_no_conversation(&ledger(vec![native("a"), app("b")])), "native and app records");
    for k in [Kind::Text, Kind::Settings, Kind::Title, Kind::Usage] {
        assert!(!formats::holds_no_conversation(&ledger(vec![native("a"), Event::new("b", k)])), "{k:?} with a native record");
    }
}
