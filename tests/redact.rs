//! Redaction by appended fact (SPEC §11.1), end to end: a store
//! redacts an event, every primary export leaves the event and its native
//! record out while the ledger keeps both, Claude Code's `parentUuid` chain
//! stays whole, an unredaction brings the export back byte for byte, and
//! every refusal appends nothing.
//!
//! `PL_REDACT_FORCE` forces a guard red (never set in CI):
//!
//! | value | what it does | what must fail |
//! |---|---|---|
//! | `bypass` | every export here calls the adapter's own `export` (the inner format type), not the entry point | the redacted event is in the export |
#![cfg(feature = "store")]

use protoledger::formats::{self, ExportOptions, ImportOptions, Output};
use protoledger::ledger::{RedactError, is_fact};
use protoledger::store::Store;
use protoledger::{Event, Kind, Ledger, LedgerSet};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

fn fixture(p: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(p)
}

fn force() -> Option<String> {
    std::env::var("PL_REDACT_FORCE").ok().filter(|s| !s.is_empty())
}

/// Export `set` as `format` the way every caller does: through the format
/// [`formats::by_name`] hands out, whose `export` is the entry point.
/// `PL_REDACT_FORCE=bypass` calls the adapter's own type instead.
fn export(format: &str, set: &LedgerSet) -> Output {
    let opts = ExportOptions::standard();
    let ex = if force().as_deref() == Some("bypass") {
        eprintln!("redact: PL_REDACT_FORCE=bypass: {format} exported by its adapter, not the entry point");
        match format {
            "claude-code" => formats::Format::export(&formats::claude_code::ClaudeCode, set, &opts),
            "codex" => formats::Format::export(&formats::codex::Codex, set, &opts),
            "opencode" => formats::Format::export(&formats::opencode::Opencode, set, &opts),
            "protoledger" => formats::Format::export(&formats::protoledger::Protoledger, set, &opts),
            _ => unreachable!(),
        }
    } else {
        formats::by_name(format).unwrap().export(set, &opts)
    };
    ex.unwrap_or_else(|e| panic!("{format} export: {e:#}")).output
}

fn contains(out: &Output, needle: &str) -> bool {
    out.files.values().any(|b| b.windows(needle.len()).any(|w| w == needle.as_bytes()))
}

fn ledger(s: &Store, agent: &str) -> Ledger {
    Ledger::read(&s.ledger_path(agent).unwrap()).unwrap()
}

/// Every ledger of the store, as an export reads them.
fn set_of(s: &Store) -> LedgerSet {
    let mut set = LedgerSet::new();
    for a in s.agents().unwrap() {
        set.insert(ledger(s, &a.id));
    }
    set
}

/// A text the export spells as the ledger does (printable ASCII that no JSON
/// writer escapes), long enough to be found only where the event is.
fn plain(t: &str) -> bool {
    t.len() >= 3 && t.chars().all(|c| (c.is_ascii_graphic() || c == ' ') && !"\"\\<>&".contains(c))
}

/// How often `needle` occurs in the files of `out`.
fn count(out: &Output, needle: &str) -> usize {
    out.files.values().map(|b| b.windows(needle.len()).filter(|w| *w == needle.as_bytes()).count()).sum()
}

/// Model-visible text events of `l`'s view that came from a native record,
/// whose text occurs exactly once in `out`; when there are none (opencode
/// writes a text again in other records of the same message), those whose
/// text occurs at all.
fn candidates<'a>(l: &'a Ledger, out: &Output) -> Vec<&'a Event> {
    let all: Vec<&Event> = l
        .projected_view()
        .into_iter()
        .filter(|e| e.kind == Kind::Text && record_of(l, e).is_some())
        .filter(|e| e.str("text").is_some_and(plain))
        .filter(|e| count(out, e.str("text").unwrap()) >= 1)
        .collect();
    let once: Vec<&Event> = all.iter().copied().filter(|e| count(out, e.str("text").unwrap()) == 1).collect();
    if once.is_empty() { all } else { once }
}

/// The text of the native record `e` came from (carried by its part 0).
fn record_of(l: &Ledger, e: &Event) -> Option<String> {
    let n = e.native.as_ref()?;
    l.events
        .iter()
        .filter_map(|x| x.native.as_ref())
        .filter(|x| x.format == n.format && x.id == n.id && x.part == 0)
        .find_map(|x| x.record_text().map(str::to_string))
}

/// The bytes of the line of a ledger file that holds event `id`.
fn held_line(bytes: &[u8], id: &str) -> Option<Vec<u8>> {
    bytes.split(|b| *b == b'\n').find(|l| serde_json::from_slice::<Value>(l).ok().is_some_and(|v| v["id"] == id)).map(<[u8]>::to_vec)
}

fn store_with(dir: &Path, format: &str, path: &Path) -> Store {
    let mut s = Store::open(dir.join("store")).unwrap();
    s.import(format, path).unwrap();
    s
}

struct Case {
    format: &'static str,
    fixture: &'static str,
}

const PRIMARIES: [Case; 3] = [
    Case { format: "claude-code", fixture: "claude-code/file-edit/native" },
    Case { format: "codex", fixture: "codex/legacy-0.130-mixed-tools/native" },
    Case { format: "opencode", fixture: "opencode/v1-export-1.19.3-generated/native" },
];

/// What a Claude Code transcript's `parentUuid` chain says, as Claude Code
/// resumes it: from the last conversation line back to a root. `Err` names
/// the first `parentUuid` that names no line of the file, or a conversation
/// line the chain does not reach.
fn claude_chain(bytes: &[u8]) -> Result<Vec<String>, String> {
    let mut parent: HashMap<String, Option<String>> = HashMap::new();
    let mut conv: Vec<String> = Vec::new();
    for line in std::str::from_utf8(bytes).unwrap().lines() {
        let v: Value = serde_json::from_str(line).map_err(|e| format!("{e}: {line}"))?;
        let Some(u) = v["uuid"].as_str() else { continue };
        parent.insert(u.to_string(), v["parentUuid"].as_str().map(str::to_string));
        if matches!(v["type"].as_str(), Some("user" | "assistant")) {
            conv.push(u.to_string());
        }
    }
    let leaf = conv.last().ok_or("no conversation line")?.clone();
    let mut chain = vec![];
    let mut cur = Some(leaf);
    while let Some(u) = cur {
        let p = parent.get(&u).ok_or_else(|| format!("parentUuid {u} names no line of the file"))?;
        chain.push(u);
        cur = p.clone();
    }
    let on: HashSet<&String> = chain.iter().collect();
    if let Some(off) = conv.iter().find(|u| !on.contains(u)) {
        return Err(format!("conversation line {off} is not on the chain from the last line"));
    }
    chain.reverse();
    Ok(chain)
}

fn claude_session(out: &Output) -> &[u8] {
    out.files.iter().find(|(k, _)| !k.contains('/') && k.ends_with(".jsonl")).map(|(_, v)| v.as_slice()).expect("a session file")
}

/// The chain checker can fail: a transcript with a line cut out of its
/// middle is refused, and the whole one is not (the sentinel for every
/// chain assertion below).
#[test]
fn claude_chain_check_refuses_a_cut_transcript_and_accepts_the_whole_one() {
    let bytes = std::fs::read(fixture("claude-code/resume/native/eb7727c8-dbf6-48df-acbe-b4153a706127.jsonl")).unwrap();
    let chain = claude_chain(&bytes).expect("the real transcript is one chain");
    assert!(chain.len() > 4);
    let cut: Vec<&str> = std::str::from_utf8(&bytes).unwrap().lines().filter(|l| !l.contains(&format!("\"uuid\":\"{}\"", chain[2]))).collect();
    let err = claude_chain(cut.join("\n").as_bytes()).expect_err("a cut chain");
    assert!(err.contains(&chain[2]), "{err}");
}

/// For every primary format: redact one model-visible event in the middle of
/// the conversation, then export. The event's text and its native record are
/// in no file of the export (rendered or carried), the ledger still holds the
/// event byte for byte, the rest of the conversation is still exported and
/// re-imports as itself, and an unredaction brings the export back byte for
/// byte. Opposed: the event was in the export before (the search finds it),
/// and a neighbour stays (a filter that drops everything fails).
#[test]
fn redacted_event_is_in_no_primary_export_the_ledger_keeps_it_and_unredact_restores_the_export() {
    for case in &PRIMARIES {
        let f = case.format;
        let dir = tempfile::tempdir().unwrap();
        let mut s = store_with(dir.path(), f, &fixture(case.fixture));
        let set = set_of(&s);
        let before = export(f, &set);
        // The first agent with a candidate and another text of its view in
        // the export; the target is the candidate in the middle, the
        // neighbour the first other text.
        let (agent, target, neighbour) = set
            .iter()
            .filter_map(|l| {
                let c = candidates(l, &before);
                let target = c.get(c.len() / 2)?;
                let other = l
                    .projected_view()
                    .into_iter()
                    .find(|e| e.kind == Kind::Text && e.id != target.id && e.str("text").is_some_and(|t| plain(t) && contains(&before, t)))?;
                Some((l.id().to_string(), (*target).clone(), other.clone()))
            })
            .next()
            .unwrap_or_else(|| {
                let texts: Vec<(String, Option<String>, bool)> = set
                    .iter()
                    .flat_map(|l| {
                        l.projected_view()
                            .into_iter()
                            .filter(|e| e.kind == Kind::Text)
                            .map(|e| (e.id.clone(), e.str("text").map(str::to_string), e.native.as_ref().is_some_and(|n| n.record_text().is_some())))
                    })
                    .collect();
                panic!("{f}: no ledger of {} has a candidate text and another: {texts:?}", case.fixture)
            });
        let text = target.str("text").unwrap().to_string();
        let record = record_of(set.get(&agent).unwrap(), &target).unwrap();
        let near = neighbour.str("text").unwrap().to_string();
        assert!(contains(&before, &text), "{f}: sentinel: the search finds the text before the redaction");
        let verbatim = contains(&before, &record);
        if f != "opencode" {
            assert!(verbatim, "{f}: sentinel: the native record is written back verbatim before the redaction");
        }
        let len = ledger(&s, &agent).events.len();
        let file = s.ledger_path(&agent).unwrap();
        let bytes0 = std::fs::read(&file).unwrap();
        let line0 = held_line(&bytes0, &target.id).expect("sentinel: the event's line is found in the ledger file before");

        s.redact(&agent, std::slice::from_ref(&target.id), "a secret pasted by mistake").unwrap();
        let held = ledger(&s, &agent);
        assert_eq!(held.events.len(), len + 1, "{f}: one fact appended");
        let kept = held.events.iter().find(|e| e.id == target.id).expect("the ledger still holds the redacted event");
        assert_eq!(kept, &target, "{f}: the redacted event is unchanged in the ledger");
        // Byte for byte: the file before is the start of the file
        // after, and the event's own line is the same bytes, so no line was
        // re-serialised (equal parsed events would not see that).
        let bytes1 = std::fs::read(&file).unwrap();
        assert!(bytes1.len() > bytes0.len() && bytes1.starts_with(&bytes0), "{f}: the ledger file before the redaction is not a byte prefix of the file after");
        assert_eq!(held_line(&bytes1, &target.id).as_deref(), Some(line0.as_slice()), "{f}: the redacted event's line in the ledger file changed");
        assert!(held.redacted().contains(&target.id));

        let after = export(f, &set_of(&s));
        // Where the text occurred once, it is gone; where other records
        // repeat it (opencode), it occurs less often, and the redacted
        // record's own id is gone.
        let (n0, n1) = (count(&before, &text), count(&after, &text));
        assert!(n1 < n0 && (n0 > 1 || n1 == 0), "{f}: the redacted event's text {text:?} is in the export ({n1} times; {n0} before)");
        assert!(!contains(&after, &record), "{f}: the redacted event's native record is in the export");
        if f == "opencode" {
            let part = target.native.as_ref().and_then(|n| n.id.clone()).unwrap();
            assert!(contains(&before, &part), "{f}: sentinel: the part id {part} is in the export before");
            assert!(!contains(&after, &part), "{f}: the redacted part {part} is in the export");
        }
        assert!(!contains(&after, "protoledger/redacted"), "{f}: the redaction fact is in the export");
        assert!(contains(&after, &near), "{f}: the neighbour {near:?} is not in the export (everything dropped)");

        // Re-imported, the export holds the conversation without the event.
        let t = tempfile::tempdir().unwrap();
        let path = protoledger::roundtrip::materialise(&after, t.path()).unwrap();
        let back = formats::by_name(f).unwrap().import(&path, &ImportOptions::standard()).unwrap();
        let ids: HashSet<&str> = back.set.iter().flat_map(|l| l.events.iter().map(|e| e.id.as_str())).collect();
        let again: Vec<&Event> = back.set.iter().flat_map(|l| l.events.iter()).filter(|e| e.id == target.id).collect();
        assert!(!ids.contains(target.id.as_str()), "{f}: the redacted event came back on re-import: {target:?} as {again:?}");
        assert!(ids.contains(neighbour.id.as_str()), "{f}: the neighbour did not come back on re-import");
        // Every event of the agent's projected view comes back under its own
        // id, the ones after the redacted event included (a filter that drops
        // the rest of the conversation fails; so does an export whose later
        // records would be read back as other events).
        let view: Vec<&Event> = held.projected_view();
        let lost: Vec<&str> = view.iter().map(|e| e.id.as_str()).filter(|id| !ids.contains(id)).collect();
        assert!(lost.is_empty(), "{f}: {} of {} events of the projected view did not come back under their ids: {lost:?}", lost.len(), view.len());
        assert!(view.iter().any(|e| e.seq > target.seq), "{f}: sentinel: the view holds events after the redacted one");
        if f == "claude-code" {
            let chain = claude_chain(claude_session(&after)).unwrap_or_else(|e| panic!("claude-code: the chain is broken: {e}"));
            assert!(!chain.contains(target.native.as_ref().unwrap().id.as_ref().unwrap()));
            claude_chain(claude_session(&before)).expect("sentinel: the export before the redaction is one chain");
        }

        s.unredact(&agent, std::slice::from_ref(&target.id)).unwrap();
        assert!(ledger(&s, &agent).redacted().is_empty());
        let again = export(f, &set_of(&s));
        assert!(again == before, "{f}: the export after unredaction is not byte-identical to the one before the redaction");
        eprintln!("redact: {f}: {} redacted ({} bytes of record) and restored", target.id, record.len());
    }
}

/// SPEC §11.1 refusals, through the store: an id the ledger lacks, an empty
/// list, a redaction of a redaction (or unredaction) fact; and an
/// unredaction of nothing or of an unknown id. Each appends nothing,
/// even when the other ids it lists would pass. Opposed: a valid redaction
/// and unredaction of the same ids is appended.
#[test]
fn every_refused_redaction_appends_nothing_and_a_valid_one_appends_one_fact() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = store_with(dir.path(), "protoledger", &fixture("protoledger/minimal/native"));
    let agent = s.agents().unwrap()[0].id.clone();
    let first = ledger(&s, &agent).events[0].id.clone();
    let len = |s: &Store| ledger(s, &agent).events.len();
    let n0 = len(&s);
    let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let refused = |r: anyhow::Result<protoledger::Appended>| r.expect_err("refused").downcast::<RedactError>().expect("a RedactError");

    assert_eq!(refused(s.redact(&agent, &ids(&[&first, "no-such-event"]), "x")), RedactError::Unknown("no-such-event".into()));
    assert_eq!(refused(s.redact(&agent, &[], "x")), RedactError::Empty("protoledger/redacted"));
    assert_eq!(refused(s.unredact(&agent, &[])), RedactError::Empty("protoledger/unredacted"));
    assert_eq!(refused(s.unredact(&agent, &ids(&["no-such-event"]))), RedactError::Unknown("no-such-event".into()));
    assert_eq!(len(&s), n0, "a refusal appended something");

    let at = s.redact(&agent, &ids(&[&first]), "x").unwrap();
    let fact = ledger(&s, &agent).events.last().unwrap().clone();
    assert_eq!((at.seq as usize, fact.str("type")), (n0 + 1, Some("protoledger/redacted")));
    assert!(matches!(refused(s.redact(&agent, &ids(&[&fact.id]), "x")), RedactError::Fact(id, "protoledger/redacted") if id == fact.id));
    s.unredact(&agent, &ids(&[&first])).unwrap();
    let un = ledger(&s, &agent).events.last().unwrap().id.clone();
    assert!(matches!(refused(s.redact(&agent, &ids(&[&un]), "x")), RedactError::Fact(id, "protoledger/unredacted") if id == un));
    assert_eq!(len(&s), n0 + 2, "only the two valid facts were appended");
    // Unredacting a fact, or an event that is not redacted, is no refusal.
    s.unredact(&agent, &ids(&[&fact.id, &first])).unwrap();
    assert_eq!(len(&s), n0 + 3);
}

/// A ledger holding §11 facts (message, delivered, cancelled, withdrawn,
/// projected, and a redaction of nothing the view holds) exports without
/// them in every format but `protoledger`, whose export is the ledger itself,
/// byte for byte.
#[test]
fn section_11_facts_are_left_out_of_every_export_but_protoledger_s_which_is_the_ledger_itself() {
    let src = Ledger::read(&fixture("protoledger/minimal/native/hello-1.jsonl")).unwrap();
    let mut l = src.clone();
    let app = |id: &str, ty: &str, data: Value| Event::new(id, Kind::App).with("type", ty).with("data", data);
    let first = l.events[0].id.clone();
    l.append(app("m1", "protoledger/message", serde_json::json!({"message_id": "msg-1", "text": "queued words", "meta": {}}))).unwrap();
    l.append(app("d1", "protoledger/cancelled", serde_json::json!({"message_ids": ["msg-1"], "reason": "interrupted"}))).unwrap();
    l.append(app("d2", "protoledger/delivered", serde_json::json!({"message_ids": ["msg-1"]}))).unwrap();
    l.append(app("w1", "protoledger/withdrawn", serde_json::json!({"message_ids": ["msg-1"], "reason": "x"}))).unwrap();
    l.append(app("p1", "protoledger/projected", serde_json::json!({"binding": "b", "bytes": 1, "sha256": "00", "through": null}))).unwrap();
    l.append(app("r1", "protoledger/redacted", serde_json::json!({"events": ["m1"], "reason": "a fact"}))).unwrap();
    assert!(l.events.iter().filter(|e| is_fact(e)).count() == 6 && l.redacted().contains("m1"));
    let _ = first;
    let set = LedgerSet::single(l.clone());
    let plain_set = LedgerSet::single(src);
    let mut exported = 0;
    for f in formats::all() {
        if !f.variants().iter().any(|v| v.export) {
            continue;
        }
        let ex = match f.export(&set, &ExportOptions::standard()) {
            Ok(ex) => ex,
            Err(e) if !cfg!(feature = "sqlite") && format!("{e:#}").contains("`sqlite` feature") => continue,
            Err(e) => panic!("{}: {e:#}", f.name()),
        };
        exported += 1;
        if f.name() == "protoledger" {
            assert_eq!(
                ex.output.files.values().next().map(|b| String::from_utf8(b.clone()).unwrap()),
                Some(l.to_jsonl()),
                "protoledger: not the ledger itself"
            );
            continue;
        }
        for ty in protoledger::ledger::FACTS {
            assert!(!contains(&ex.output, ty), "{}: the export holds a {ty} fact", f.name());
        }
        assert!(!contains(&ex.output, "queued words"), "{}: the export holds a message fact's text", f.name());
        // Leaving the facts out is the whole difference, and is no loss (§11.1).
        let plain = f.export(&plain_set, &ExportOptions::standard()).unwrap();
        assert!(plain.output == ex.output && plain.losses == ex.losses, "{}: the facts changed the export", f.name());
    }
    assert!(exported >= 15, "only {exported} formats exported");
}

/// The binary: `store redact` appends the fact and names it; an unknown id
/// fails, naming the id, and appends nothing; `store unredact` withdraws it.
#[cfg(feature = "cli")]
#[test]
fn store_redact_and_unredact_cli() {
    use std::process::Command;
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let s = store_with(dir.path(), "protoledger", &fixture("protoledger/minimal/native"));
    let agent = s.agents().unwrap()[0].id.clone();
    let first = ledger(&s, &agent).events[0].id.clone();
    drop(s);
    let run = |args: &[&str]| Command::new(env!("CARGO_BIN_EXE_protoledger")).args(args).output().unwrap();
    let st = store.to_str().unwrap();
    let n0 = Ledger::read(&store.join(format!("ledgers/{agent}.jsonl"))).unwrap().events.len();
    let bad = run(&["store", "redact", st, &agent, &first, "no-such-event", "--reason", "x"]);
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("no-such-event"), "{}", String::from_utf8_lossy(&bad.stderr));
    let held = || Ledger::read(&store.join(format!("ledgers/{agent}.jsonl"))).unwrap();
    assert_eq!(held().events.len(), n0);
    let ok = run(&["store", "redact", st, &agent, &first, "--reason", "a secret"]);
    assert!(ok.status.success(), "{}", String::from_utf8_lossy(&ok.stderr));
    let l = held();
    assert_eq!(l.events.len(), n0 + 1);
    assert_eq!(l.events.last().unwrap().body["data"], serde_json::json!({"events": [first.clone()], "reason": "a secret"}));
    assert!(l.redacted().contains(&first));
    let ok = run(&["store", "unredact", st, &agent, &first]);
    assert!(ok.status.success(), "{}", String::from_utf8_lossy(&ok.stderr));
    assert!(held().redacted().is_empty());
}

/// The message ids an opencode export holds, and every `parentID` naming a
/// message it writes: `(ids, parents)`.
fn opencode_parents(out: &Output) -> (HashSet<String>, Vec<String>) {
    fn walk(v: &Value, ids: &mut HashSet<String>, parents: &mut Vec<String>) {
        match v {
            Value::Object(o) => {
                if let Some(id) = o.get("id").and_then(Value::as_str).filter(|i| i.starts_with("msg_")) {
                    ids.insert(id.to_string());
                }
                if let Some(p) = o.get("parentID").and_then(Value::as_str).filter(|i| i.starts_with("msg_")) {
                    parents.push(p.to_string());
                }
                o.values().for_each(|x| walk(x, ids, parents));
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, ids, parents)),
            _ => {}
        }
    }
    let (mut ids, mut parents) = (HashSet::new(), Vec::new());
    for b in out.files.values() {
        let t = String::from_utf8_lossy(b);
        match serde_json::from_str::<Value>(&t) {
            Ok(v) => walk(&v, &mut ids, &mut parents),
            Err(_) => t.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).for_each(|v| walk(&v, &mut ids, &mut parents)),
        }
    }
    (ids, parents)
}

/// The `parentID`s of `out` that name a message `out` does not hold.
fn dangling(out: &Output) -> Vec<String> {
    let (ids, parents) = opencode_parents(out);
    parents.into_iter().filter(|p| !ids.contains(p)).collect()
}

/// What opencode needs across a gap: an assistant message names the user
/// message it answers (`parentID`). Redacting a whole user message (the
/// events carrying its record, and its text part)
/// leaves the export without that message; its answers then name the
/// nearest earlier user message, so every `parentID` the export writes names
/// a message of the export. With no earlier user message, the answer keeps
/// naming the redacted one and the export says so as a loss. Sentinels:
/// before the redaction the export is whole and holds the message; the
/// first user message, and the other messages, stay. For two v1 exports
/// (opencode 1.19.3, 1.2.21). opencode 2.x needs nothing: its messages
/// carry no `parentID` (none in `v2-transfer-2.0.31-generated`).
#[test]
fn opencode_parent_ids_name_messages_of_the_export_after_a_whole_user_message_is_redacted() {
    for (fx, session) in [
        ("opencode/v1-export-1.19.3-generated/native", "ses_eb8d515deffeLpefMnb31VAQpy"),
        ("opencode/v1-export-1.2.21-generated/native", "ses_eb8d6b52fffeC2XYPO9qzTJrOG"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut s = store_with(dir.path(), "opencode", &fixture(fx));
        let set = set_of(&s);
        let before = export("opencode", &set);
        let (ids0, parents0) = opencode_parents(&before);
        let dangling0 = dangling(&before);
        assert!(dangling0.is_empty(), "{fx}: sentinel: the export before the redaction is whole: {dangling0:?}");
        let l = set.get(session).unwrap_or_else(|| panic!("{fx}: no session {session}"));
        let users: Vec<String> =
            l.events.iter().filter(|e| e.kind == Kind::Text && e.str("role") == Some("user") && e.group.is_none()).filter_map(|e| e.turn.clone()).collect();
        assert!(users.len() >= 2, "{fx}: two user messages: {users:?}");
        // The message's own record (v1: its `settings` event; v2: the text)
        // and its user text part.
        let whole_of = |msg: &str| -> Vec<String> {
            l.events
                .iter()
                .filter(|e| {
                    e.native.as_ref().is_some_and(|n| n.id.as_deref() == Some(msg))
                        || (e.kind == Kind::Text && e.str("role") == Some("user") && e.turn.as_deref() == Some(msg))
                })
                .map(|e| e.id.clone())
                .collect()
        };
        // The second user message: its answers re-point at the first.
        let msg = users[1].clone();
        let whole = whole_of(&msg);
        assert!(ids0.contains(&msg) && !whole.is_empty(), "{fx}: sentinel: {msg} is in the export before: {whole:?}");
        assert!(parents0.contains(&msg), "{fx}: sentinel: an answer names {msg} as its parentID before");
        s.redact(l.id(), &whole, "a whole message").unwrap();
        let after = export("opencode", &set_of(&s));
        let (ids1, _) = opencode_parents(&after);
        let dangling1 = dangling(&after);
        assert!(!ids1.contains(&msg), "{fx}: the redacted user message {msg} is in the export");
        let gone: Vec<&String> = ids0.iter().filter(|i| **i != msg && !ids1.contains(*i)).collect();
        assert!(ids1.contains(&users[0]) && gone.is_empty(), "{fx}: the other messages are in the export: {gone:?} are not");
        assert!(dangling1.is_empty(), "{fx}: parentID names a message the export does not hold: {dangling1:?}");
        // Then the first as well: nothing earlier to name; said as a loss.
        s.redact(l.id(), &whole_of(&users[0]), "the first too").unwrap();
        let ex = formats::by_name("opencode").unwrap().export(&set_of(&s), &ExportOptions::standard()).unwrap();
        let dangling2 = dangling(&ex.output);
        assert!(!dangling2.is_empty(), "{fx}: sentinel: with no user message left, an answer names a redacted one");
        assert!(ex.losses.0.iter().any(|x| x.code == "parent-message-redacted"), "{fx}: no loss names the dangling parentID: {:?}", ex.losses);
        eprintln!("redact: opencode: {fx}: {msg} ({} events) redacted; {} messages left, every parentID names one", whole.len(), ids1.len());
    }
}

/// Pinned: the `protoledger` exporter renders abandoned branches (it
/// writes every event), and a ledger holding a redaction does not export as
/// itself: the export is the ledger minus exactly its redacted events, on the
/// active path and on the abandoned branch alike, with every §11 fact kept
/// where it stands, `after` re-pointed past a left-out event, and `seq`
/// renumbered. So a `protoledger` fixture holding redactions is not
/// byte-exact on its self trip (SPEC §7.1); none is in `fixtures/`, and one
/// would have to say so in its `expect.json` (`self_exact: false`). Read
/// back, the fact names nothing the export holds and is ignored.
#[test]
fn protoledger_export_of_a_redacted_ledger_is_the_ledger_minus_the_redacted_events_branches_and_facts_kept() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = store_with(dir.path(), "protoledger", &fixture("protoledger/branch-rewind/native"));
    let agent = s.agents().unwrap()[0].id.clone();
    let src = ledger(&s, &agent);
    assert!(src.is_branched(), "sentinel: the fixture has an abandoned branch");
    let on_path: HashSet<&str> = src.active_path().into_iter().map(|e| e.id.as_str()).collect();
    assert!(on_path.contains("rewind-1:a:1") && !on_path.contains("rewind-1:c:1"), "sentinel: a:1 is on the active path and c:1 abandoned");
    let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    s.redact(&agent, &ids(&["rewind-1:a:1", "rewind-1:c:1"]), "x").unwrap();
    let held = ledger(&s, &agent);
    let out = export("protoledger", &LedgerSet::single(held.clone()));
    let back = Ledger::from_jsonl(std::str::from_utf8(out.files.values().next().unwrap()).unwrap()).unwrap().0;
    let got: Vec<(&str, Option<&str>, u64)> = back.events.iter().map(|e| (e.id.as_str(), e.after.as_deref(), e.seq)).collect();
    let fact = held.events.last().unwrap().id.clone();
    let want: Vec<(&str, Option<&str>, u64)> = vec![
        ("rewind-1:u:1", None, 1),
        ("rewind-1:u:2", None, 2),
        ("rewind-1:r:1", None, 3),
        ("rewind-1:a:2", None, 4),
        ("rewind-1:u:3", Some("rewind-1:u:1"), 5),
        ("rewind-1:a:3", None, 6),
        (fact.as_str(), None, 7),
    ];
    assert_eq!(got, want);
    assert_ne!(out.files.values().next().unwrap().as_slice(), held.to_jsonl().as_bytes(), "a redacted ledger exported as itself");
    assert!(back.redacted().is_empty(), "read back, the fact names events the export does not hold, and is ignored");
}

/// The lines of a Claude Code file, parsed, with the byte offset each
/// starts at.
fn claude_lines(bytes: &[u8]) -> Vec<(usize, Value)> {
    let mut at = 0;
    let mut out = vec![];
    for line in std::str::from_utf8(bytes).unwrap().split_inclusive('\n') {
        out.push((at, serde_json::from_str(line.trim_end_matches('\n')).unwrap_or(Value::Null)));
        at += line.len();
    }
    out
}

/// The ids of every event of every ledger of `set`.
fn ids_of(set: &LedgerSet) -> HashSet<String> {
    set.iter().flat_map(|l| l.events.iter().map(|e| e.id.clone())).collect()
}

/// Every event of `agent`'s ledger read from the Claude Code line whose
/// native id is `line`.
fn events_of_line(l: &Ledger, line: &str) -> Vec<String> {
    l.events.iter().filter(|e| e.native.as_ref().is_some_and(|n| n.format == "claude-code" && n.id.as_deref() == Some(line))).map(|e| e.id.clone()).collect()
}

/// SPEC §11.1, a branch start: a line Claude Code wrote after a
/// rewind continues from an earlier line than the one before it (its
/// `parentUuid` goes back, and its event continues from an earlier one,
/// `after`). When the line it continues from is redacted, it continues from
/// the last line written at that earlier event, on its own branch, not from
/// the last line written before it, which is the tip of the branch it
/// rewound from. Real transcript (`_live-rewind-2.1.280`, Claude Code's own
/// rewind). Sentinels: the branch start names the redacted line before; the
/// two candidate parents are different lines of the export. Every
/// `parentUuid` the export writes names a line of it.
#[test]
fn claude_branch_start_whose_parent_is_redacted_continues_on_its_own_branch_not_from_the_other_tip() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = store_with(dir.path(), "claude-code", &fixture("claude-code/_live-rewind-2.1.280/native"));
    let before = export("claude-code", &set_of(&s));
    let lines = claude_lines(claude_session(&before));
    // The branch start: a conversation line whose parent is not the line
    // with a uuid just before it.
    let mut last: Option<String> = None;
    let mut branch: Option<(String, String, String)> = None; // (uuid, parent, tip before it)
    for (_, v) in &lines {
        let Some(u) = v["uuid"].as_str() else { continue };
        if let (Some(p), Some(tip)) = (v["parentUuid"].as_str(), last.as_ref())
            && p != tip
            && v["type"] == "user"
        {
            branch = Some((u.to_string(), p.to_string(), tip.clone()));
        }
        last = Some(u.to_string());
    }
    let (start, parent, tip) = branch.expect("sentinel: the transcript holds a rewind (a line whose parent is not the line before it)");
    let grand = lines.iter().find(|(_, v)| v["uuid"] == parent.as_str()).and_then(|(_, v)| v["parentUuid"].as_str()).unwrap().to_string();
    assert_ne!(grand, tip, "sentinel: the two candidate parents are different lines");
    let agent = s.agents().unwrap().into_iter().find(|a| !events_of_line(&ledger(&s, &a.id), &parent).is_empty()).expect("the ledger holding the line").id;
    let ids = events_of_line(&ledger(&s, &agent), &parent);
    s.redact(&agent, &ids, "a secret").unwrap();

    let after = export("claude-code", &set_of(&s));
    let out = claude_lines(claude_session(&after));
    let uuids: HashSet<&str> = out.iter().filter_map(|(_, v)| v["uuid"].as_str()).collect();
    assert!(
        !uuids.contains(parent.as_str()) && uuids.contains(grand.as_str()) && uuids.contains(tip.as_str()),
        "sentinel: the redacted line is gone, both candidates are written"
    );
    let at = out.iter().find(|(_, v)| v["uuid"] == start.as_str()).map(|(_, v)| v["parentUuid"].clone()).expect("the branch start is written");
    assert_eq!(
        at,
        Value::from(grand.clone()),
        "the branch start continues from {at} (the other branch's tip is {tip}); it should continue on its own branch, from {grand}"
    );
    let dangling: Vec<&str> = out.iter().filter_map(|(_, v)| v["parentUuid"].as_str()).filter(|p| !uuids.contains(p)).collect();
    assert!(dangling.is_empty(), "parentUuid names no line of the export: {dangling:?}");
}

/// SPEC §11.1: redacting part 1 of a real two-part Claude Code line
/// (one assistant line holding a thinking block and then a text block,
/// `transession-2.1.71`; part 0 is the line's settings event, which carries
/// the record, part 1 the thinking, part 2 the text) leaves that line as
/// Claude Code wrote it out of the export, and the redacted thinking with
/// it; part 2 (the text) is still exported, rendered from its neutral body.
/// Sentinels: before the redaction the line is written back verbatim and
/// holds both texts.
#[test]
fn claude_two_part_line_with_part_1_redacted_is_not_in_the_export_and_its_other_part_is() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = store_with(dir.path(), "claude-code", &fixture("claude-code/transession-2.1.71/native"));
    let set = set_of(&s);
    let before = export("claude-code", &set);
    let (agent, part1, part2) = set
        .iter()
        .find_map(|l| {
            let p1 = l.events.iter().find(|e| e.kind == Kind::Reasoning && e.native.as_ref().is_some_and(|n| n.format == "claude-code" && n.part == 1))?;
            let id = p1.native.as_ref().unwrap().id.clone();
            let p2 = l.events.iter().find(|e| e.kind == Kind::Text && e.native.as_ref().is_some_and(|n| n.id == id && n.part == 2))?;
            Some((l.id().to_string(), p1.clone(), p2.clone()))
        })
        .expect("sentinel: the fixture holds one line of thinking (part 1) then text (part 2)");
    let record = record_of(set.get(&agent).unwrap(), &part1).expect("part 0 carries the line");
    // As the export spells them (JSON string contents).
    let spelled = |t: &str| serde_json::to_string(t).unwrap().trim_matches('"').to_string();
    let thinking = spelled(part1.str("text").unwrap());
    let text = spelled(part2.str("text").unwrap());
    assert!(record.contains("\"type\":\"thinking\"") && record.contains("\"type\":\"text\""), "sentinel: one line of both blocks: {record}");
    assert!(contains(&before, &record), "sentinel: the line is written back verbatim before the redaction");
    assert!(contains(&before, &thinking) && contains(&before, &text), "sentinel: both texts are in the export before");

    s.redact(&agent, std::slice::from_ref(&part1.id), "part 1 only").unwrap();
    let after = export("claude-code", &set_of(&s));
    assert!(!contains(&after, &record), "the line with a redacted part is written back as Claude Code wrote it");
    assert!(!contains(&after, &thinking), "the redacted part's thinking is in the export");
    assert!(contains(&after, &text), "part 2's text {text:?} is not in the export (the whole line dropped)");
    claude_chain(claude_session(&after)).unwrap_or_else(|e| panic!("the chain is broken: {e}"));
}

/// SPEC §11.1: a Claude Code line with no uuid is named by its byte offset
/// (`@<n>`). Behind a redacted line it lands at another offset; when it also
/// carries foreign events (here, two turn starts added to the ledger after
/// the transcript, which have no line and ride on its last line), it is
/// written with one carrier,
/// and read back, its own events come back under their own ids, and the
/// foreign events too. Sentinels: the line does land off its offset, and it
/// does carry the foreign events.
#[test]
fn claude_uuidless_line_off_its_offset_carrying_foreign_events_reads_back_under_its_own_ids() {
    let path = fixture("claude-code/_live-rewind-2.1.280/native");
    let mut set = formats::by_name("claude-code").unwrap().import(&path, &ImportOptions::standard()).unwrap().set;
    let main = set.iter().find(|l| l.agent().parent.is_none()).unwrap().id().to_string();
    let mut l = set.get(&main).unwrap().clone();
    let bytes =
        std::fs::read(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()).find(|p| p.extension().is_some_and(|x| x == "jsonl")).unwrap()).unwrap();
    let src = claude_lines(&bytes);
    let (last_at, last) = src.iter().rev().find(|(_, v)| !v.is_null()).cloned().unwrap();
    assert!(last["uuid"].is_null(), "sentinel: the transcript ends on a line with no uuid");
    let anchor = format!("@{last_at}");
    let own = events_of_line(&l, &anchor);
    assert!(!own.is_empty(), "sentinel: the last line's events are named {anchor}");
    // An early conversation line to redact, so every later line moves.
    let first_conv = src.iter().find_map(|(_, v)| (v["type"] == "assistant").then(|| v["uuid"].as_str().unwrap().to_string())).unwrap();
    let struck = events_of_line(&l, &first_conv);
    l.append(Event::new("foreign-1", Kind::Turn).with("phase", "start")).unwrap();
    l.append(Event::new("foreign-2", Kind::Turn).with("phase", "start")).unwrap();
    l.append(protoledger::ledger::redaction_event(false, &struck, "x")).unwrap();
    set.insert(l.clone());

    let after = export("claude-code", &set);
    let file = claude_session(&after);
    let out = claude_lines(file);
    let (at, v) = out.iter().rev().find(|(_, v)| !v.is_null()).cloned().unwrap();
    let mut bare = v.clone();
    bare.as_object_mut().unwrap().remove("protoledger");
    assert_eq!(bare, last, "sentinel: the export ends on the transcript's last line (no line of its own for the foreign events)");
    assert_ne!(format!("@{at}"), anchor, "sentinel: the last line lands off its offset");
    let carried: Vec<&str> = v["protoledger"]["events"].as_array().map(|a| a.iter().filter_map(|e| e["id"].as_str()).collect()).unwrap_or_default();
    assert!(carried.contains(&"foreign-1") && carried.contains(&"foreign-2"), "sentinel: the last line carries the foreign events: {carried:?}");

    let t = tempfile::tempdir().unwrap();
    let back =
        formats::by_name("claude-code").unwrap().import(&protoledger::roundtrip::materialise(&after, t.path()).unwrap(), &ImportOptions::standard()).unwrap();
    let ids = ids_of(&back.set);
    let want: Vec<String> = own.iter().cloned().chain(["foreign-1".to_string(), "foreign-2".to_string()]).collect();
    let missing: Vec<&String> = want.iter().filter(|i| !ids.contains(*i)).collect();
    assert!(missing.is_empty(), "read back, these did not come back under their own ids: {missing:?}");
    let view: Vec<&Event> = l.projected_view();
    let lost: Vec<&str> = view.iter().map(|e| e.id.as_str()).filter(|i| !ids.contains(*i)).collect();
    assert!(lost.is_empty(), "{} of {} events of the projected view did not come back under their ids: {lost:?}", lost.len(), view.len());
}
