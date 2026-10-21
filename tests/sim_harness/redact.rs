//! A simulated Claude Code turn resumed from a projection that leaves a
//! redacted event out (SPEC §11.1).
//!
//! The ledger is `fixtures/claude-code/resume` (Claude Code 2.1.280, a real
//! run: its `SOURCE.md`), imported into a store, with the first answer
//! ("ok", the last line of its message) redacted. It is projected into a
//! temporary Claude Code projects directory; Claude Code resumes a session by
//! walking `parentUuid` back from the file's last conversation line, so the
//! projection must be one chain without the redacted line. The simulator then
//! appends the lines of `fixtures/claude-code/file-edit` as the resumed turn
//! (its session id replaced, its first line's null parent pointed at the
//! projection's last line, as Claude Code writes a resumed turn), one write
//! per line, while the real sync engine follows the binding on its default
//! options. The ledger gains every event of the turn once, nothing diverges,
//! the redacted event is still in the ledger and still out of the file, and
//! the whole file is one chain from the turn's last line back to the root.

use crate::common::{self, Mark, Paths};
use protoledger::formats::{self, ImportOptions};
use protoledger::store::Store;
use protoledger::sync;
use protoledger::{Event, Kind, Ledger};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const LEDGER: &str = "claude-code/resume/native";
const TURN: &str = "claude-code/file-edit/native/bcbbbce5-971a-443f-bf65-8c3ab40cc24b.jsonl";
const TURN_SESSION: &str = "bcbbbce5-971a-443f-bf65-8c3ab40cc24b";

/// The file's `parentUuid` chain from its last conversation line, as Claude
/// Code resumes it; panics on a `parentUuid` that names no line, or a
/// conversation line the chain does not reach.
fn chain(bytes: &[u8], what: &str) -> Vec<String> {
    let mut parent: HashMap<String, Option<String>> = HashMap::new();
    let mut conv = Vec::new();
    for line in std::str::from_utf8(bytes).unwrap().lines() {
        let v: Value = serde_json::from_str(line).unwrap();
        let Some(u) = v["uuid"].as_str() else { continue };
        parent.insert(u.to_string(), v["parentUuid"].as_str().map(str::to_string));
        if matches!(v["type"].as_str(), Some("user" | "assistant")) {
            conv.push(u.to_string());
        }
    }
    let mut out = vec![];
    let mut cur = conv.last().cloned();
    while let Some(u) = cur {
        let p = parent.get(&u).unwrap_or_else(|| panic!("{what}: parentUuid {u} names no line of the file: the chain is broken"));
        out.push(u);
        cur = p.clone();
    }
    // Parallel tool calls: a result's line points at its own call's line,
    // and Claude Code puts the siblings back; every line is still reached
    // from some line of the chain by its parent.
    let on: HashSet<String> = out.iter().cloned().collect();
    for u in &conv {
        let mut c = Some(u.clone());
        let mut seen = HashSet::new();
        while let Some(x) = c.filter(|x| !on.contains(x) && seen.insert(x.clone())) {
            c = parent.get(&x).cloned().flatten();
            assert!(c.is_some(), "{what}: conversation line {u} does not lead back to the chain");
        }
    }
    out
}

fn append(path: &Path, bytes: &[u8]) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(bytes).unwrap();
}

#[test]
fn claude_code_resumes_a_projection_with_a_redacted_event_once_each_no_divergence_one_chain_for_a_1s_poll_after() {
    let mut paths = Paths::new();
    let projects = paths.dir("claude/projects/-sim-redact");
    let store_dir = paths.root.join("store");
    let src = common::fixture(LEDGER);
    paths.fixture(&src);
    paths.fixture(&common::fixture(TURN));
    let mut s = Store::open(&store_dir).unwrap();
    s.import("claude-code", &src).unwrap();
    let agent = s.agents().unwrap()[0].id.clone();
    let held = Ledger::read(&s.ledger_path(&agent).unwrap()).unwrap();
    let target = held
        .events
        .iter()
        .find(|e| e.kind == Kind::Text && e.str("role") == Some("assistant") && e.str("text") == Some("ok"))
        .expect("the first answer")
        .clone();
    let line = target.native.as_ref().and_then(|n| n.id.clone()).unwrap();
    s.redact(&agent, std::slice::from_ref(&target.id), "simulated: an answer to take back").unwrap();

    let b = sync::project(&mut s, &agent, "claude-code", &projects).unwrap();
    let loc = PathBuf::from(&b.location);
    paths.wrote(&loc);
    let pre_bytes = std::fs::read(&loc).unwrap();
    let text = String::from_utf8(pre_bytes.clone()).unwrap();
    // As Claude Code reads the file: no line is the redacted one, or names
    // it as its parent. (A carrier may hold a kept event's own record as the
    // ledger has it, which names its parent there; that is not a line.)
    let naming = |text: &str| -> Vec<String> {
        text.lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| ["uuid", "parentUuid", "logicalParentUuid"].iter().any(|k| v[*k].as_str() == Some(line.as_str())))
            .map(|v| format!("{} {}", v["type"], v["uuid"]))
            .collect()
    };
    let named = naming(&text);
    let source = std::fs::read_to_string(src.join("eb7727c8-dbf6-48df-acbe-b4153a706127.jsonl")).unwrap();
    assert!(naming(&source).len() >= 2, "sentinel: in the source the predicate finds the line and the line after it");
    assert!(named.is_empty(), "the redacted line is in the projection, or named as a parent: {named:?}");
    let pre_chain = chain(&pre_bytes, "the projection");
    let leaf = pre_chain[0].clone();
    let pre_import = formats::by_name("claude-code").unwrap().import(&loc, &ImportOptions::standard()).unwrap();
    let pre_ids: Vec<String> = pre_import.set.get(&agent).unwrap().events.iter().map(|e| e.id.clone()).collect();
    assert!(!pre_ids.contains(&target.id));
    let pre_len = Ledger::read(&s.ledger_path(&agent).unwrap()).unwrap().events.len();

    let session = loc.file_stem().unwrap().to_string_lossy().to_string();
    let turn = std::fs::read_to_string(common::fixture(TURN)).unwrap().replace(TURN_SESSION, &session).replacen(
        "\"parentUuid\":null",
        &format!("\"parentUuid\":\"{leaf}\""),
        1,
    );
    let lines: Vec<String> = turn.lines().map(str::to_string).collect();
    let mut follower = common::follower(&s);
    let sim_loc = loc.clone();
    let sim = move |tx: std::sync::mpsc::Sender<Mark>| {
        for l in &lines {
            std::thread::sleep(Duration::from_millis(5));
            append(&sim_loc, format!("{l}\n").as_bytes());
        }
        let _ = tx.send(Mark::End(Instant::now()));
    };
    let want_loc = loc.clone();
    let want_agent = agent.clone();
    let pre_n = pre_ids.len();
    let want = move || {
        let imp = formats::by_name("claude-code").unwrap().import(&want_loc, &ImportOptions::standard()).unwrap();
        imp.set.get(&want_agent).unwrap().events.iter().map(|e| e.id.clone()).collect::<Vec<_>>()[pre_n..].to_vec()
    };
    let w = common::follow_turn(&mut s, follower.as_mut(), &b.binding, sim, &want, || {});
    let all = Ledger::read(&s.ledger_path(&agent).unwrap()).unwrap();
    common::no_divergence(&all.events, &w, "claude-code redacted");
    let got: Vec<Event> = all.events[pre_len..].to_vec();
    let wanted = want();
    common::once_each(&got, &wanted, "claude-code redacted", |_| false);
    common::within_bound(&wanted, &w, "claude-code redacted");

    // The resumed file is one chain, from the turn's last line through the
    // projection to the root; the redacted line is on it nowhere.
    let bytes = std::fs::read(&loc).unwrap();
    assert!(bytes.starts_with(&pre_bytes));
    let full = chain(&bytes, "the resumed file");
    assert!(full.ends_with(&pre_chain), "the turn does not continue the projection's chain");
    assert!(!full.contains(&line));
    // The ledger still holds the redacted event, and it is still redacted.
    let e = all.events.iter().find(|e| e.id == target.id).expect("the ledger lost the redacted event");
    assert_eq!(e, &target);
    assert!(all.redacted().contains(&target.id) && !all.projected_view().iter().any(|e| e.id == target.id));
    eprintln!("sim: claude-code redacted: {} new events; chain of {} lines ({} from the projection)", got.len(), full.len(), pre_chain.len());

    paths.store(&s);
    paths.assert_isolated("claude-code redacted");
}
