//! Redaction and re-projection against the simulated harness (SPEC §11.1–§11.2):
//! an API-error line Claude Code appended is redacted by an appended event,
//! re-projection leaves it out of the file Claude Code resumes from while
//! the ledger still holds it, and the next turn comes in once with no
//! divergence for protoledger's own rewrite.
//!
//! The ledger is `fixtures/claude-code/resume` (Claude Code 2.1.280, a real
//! run), projected into a temporary Claude Code projects directory. Turn 1
//! is `fixtures/claude-code/api-error` (a real run whose only reply is the
//! synthetic `isApiErrorMessage` line Claude Code writes when the API
//! refuses), its session id replaced and its first line's null parent
//! pointed at the projection's last line, as Claude Code writes a resumed
//! turn; one write per line, while the real sync engine follows the binding
//! on its default options. The error line's events are then redacted
//! (`Store::redact`, one appended fact) and the binding re-projected
//! (`sync::reproject`). Turn 2 is `fixtures/claude-code/file-edit`, resumed
//! from the re-projected file's last line, followed by the same follower.

use crate::common::{self, Mark, Paths};
use protoledger::formats::{self, ImportOptions};
use protoledger::store::Store;
use protoledger::sync;
use protoledger::{Event, Ledger};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const LEDGER: &str = "claude-code/resume/native";
const ERROR_TURN: &str = "claude-code/api-error/native/aa076ad2-c0b7-41bb-b3be-5bd38a0bbbf2.jsonl";
const ERROR_SESSION: &str = "aa076ad2-c0b7-41bb-b3be-5bd38a0bbbf2";
/// The `isApiErrorMessage` line of api-error.
const ERROR_LINE: &str = "03412cb5-2957-432f-bad2-663132335563";
const NEXT_TURN: &str = "claude-code/file-edit/native/bcbbbce5-971a-443f-bf65-8c3ab40cc24b.jsonl";
const NEXT_SESSION: &str = "bcbbbce5-971a-443f-bf65-8c3ab40cc24b";

/// The `parentUuid` chain from the file's last conversation line, as Claude
/// Code resumes it (its last line first); panics on a parent that names no line.
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
    out
}

/// The lines of `bytes`, as Claude Code reads them, that are the error line,
/// name it as their parent, or say they are an API error.
fn error_lines(bytes: &[u8]) -> Vec<String> {
    std::str::from_utf8(bytes)
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| {
            ["uuid", "parentUuid", "logicalParentUuid"].iter().any(|k| v[*k].as_str() == Some(ERROR_LINE)) || v["isApiErrorMessage"].as_bool() == Some(true)
        })
        .map(|v| format!("{} {}", v["type"], v["uuid"]))
        .collect()
}

fn append(path: &Path, bytes: &[u8]) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(bytes).unwrap();
}

/// A turn's lines as Claude Code resuming `session` after `parent` writes them.
fn turn(fx: &str, fx_session: &str, session: &str, parent: &str) -> Vec<String> {
    std::fs::read_to_string(common::fixture(fx))
        .unwrap()
        .replace(fx_session, session)
        .replacen("\"parentUuid\":null", &format!("\"parentUuid\":\"{parent}\""), 1)
        .lines()
        .map(str::to_string)
        .collect()
}

/// Run one simulated turn (`lines`, one write per line) while `follower`
/// follows the binding; returns what it watched and the ids the turn made.
fn run_turn(
    s: &mut Store,
    follower: Option<&mut sync::Follower>,
    binding: &str,
    loc: &Path,
    agent: &str,
    lines: Vec<String>,
) -> (common::Watched, Vec<String>) {
    let pre_n = formats::by_name("claude-code").unwrap().import(loc, &ImportOptions::standard()).unwrap().set.get(agent).unwrap().events.len();
    let sim_loc = loc.to_path_buf();
    let sim = move |tx: std::sync::mpsc::Sender<Mark>| {
        for l in &lines {
            std::thread::sleep(Duration::from_millis(5));
            append(&sim_loc, format!("{l}\n").as_bytes());
        }
        let _ = tx.send(Mark::End(Instant::now()));
    };
    let want_loc = loc.to_path_buf();
    let want_agent = agent.to_string();
    let want = move || {
        let imp = formats::by_name("claude-code").unwrap().import(&want_loc, &ImportOptions::standard()).unwrap();
        imp.set.get(&want_agent).unwrap().events.iter().map(|e| e.id.clone()).collect::<Vec<_>>()[pre_n..].to_vec()
    };
    let w = common::follow_turn(s, follower, binding, sim, &want, || {});
    let wanted = want();
    (w, wanted)
}

#[test]
fn claude_code_api_error_is_redacted_and_reprojected_out_ledger_keeps_it_next_turn_once_no_divergence_for_a_1s_poll_after() {
    let mut paths = Paths::new();
    let projects = paths.dir("claude/projects/-sim-reproject");
    let src = common::fixture(LEDGER);
    for f in [src.clone(), common::fixture(ERROR_TURN), common::fixture(NEXT_TURN)] {
        paths.fixture(&f);
    }
    let mut s = Store::open(paths.root.join("store")).unwrap();
    s.import("claude-code", &src).unwrap();
    let agent = s.agents().unwrap()[0].id.clone();
    let b = sync::project(&mut s, &agent, "claude-code", &projects).unwrap();
    let loc = PathBuf::from(&b.location);
    paths.wrote(&loc);
    let session = loc.file_stem().unwrap().to_string_lossy().to_string();
    let mut follower = common::follower(&s);

    // Turn 1: Claude Code appends the API-error line; sync ingests it.
    let leaf0 = chain(&std::fs::read(&loc).unwrap(), "the projection")[0].clone();
    let pre_len = Ledger::read(&s.ledger_path(&agent).unwrap()).unwrap().events.len();
    let (w1, wanted1) = run_turn(&mut s, follower.as_mut(), &b.binding, &loc, &agent, turn(ERROR_TURN, ERROR_SESSION, &session, &leaf0));
    let all = Ledger::read(&s.ledger_path(&agent).unwrap()).unwrap();
    common::no_divergence(&all.events, &w1, "turn 1 (the API error)");
    common::once_each(&all.events[pre_len..], &wanted1, "turn 1 (the API error)", |_| false);
    common::within_bound(&wanted1, &w1, "turn 1 (the API error)");
    let before = std::fs::read(&loc).unwrap();
    assert!(!error_lines(&before).is_empty(), "sentinel: the predicate finds the error line in the file Claude Code wrote");
    let error: Vec<Event> = all.events.iter().filter(|e| e.native.as_ref().and_then(|n| n.id.as_deref()) == Some(ERROR_LINE)).cloned().collect();
    assert!(!error.is_empty(), "sync did not ingest the error line");

    // Redacted by an appended event, then re-projected.
    let ids: Vec<String> = error.iter().map(|e| e.id.clone()).collect();
    let n = all.events.len();
    s.redact(&agent, &ids, "simulated: an API error Claude Code appended").unwrap();
    let held = Ledger::read(&s.ledger_path(&agent).unwrap()).unwrap();
    assert_eq!(held.events.len(), n + 1, "the redaction is one appended event");
    assert_eq!(&held.events[..n], &all.events[..], "nothing before it changed");
    let r = sync::reproject(&mut s, &b.binding).unwrap();
    assert_eq!(r.written, vec![loc.clone()], "the re-projection did not write the file Claude Code resumes from");
    assert!(r.fact.is_some());
    let after = std::fs::read(&loc).unwrap();
    let left = error_lines(&after);
    assert!(left.is_empty(), "the file Claude Code resumes from still holds the error line: {left:?}");
    let resumed = chain(&after, "the re-projected file");
    assert!(!resumed.contains(&ERROR_LINE.to_string()));
    let held = Ledger::read(&s.ledger_path(&agent).unwrap()).unwrap();
    for e in &error {
        assert_eq!(held.events.iter().find(|x| x.id == e.id), Some(e), "the ledger no longer holds the error event {} as it was", e.id);
        assert!(held.redacted().contains(&e.id) && !held.projected_view().iter().any(|x| x.id == e.id));
    }

    // Turn 2, resumed from the re-projected file: once each, no divergence.
    let pre_len = held.events.len();
    let (w2, wanted2) = run_turn(&mut s, follower.as_mut(), &b.binding, &loc, &agent, turn(NEXT_TURN, NEXT_SESSION, &session, &resumed[0]));
    let all = Ledger::read(&s.ledger_path(&agent).unwrap()).unwrap();
    common::no_divergence(&all.events, &w2, "turn 2 (after the re-projection)");
    // The rewritten file was read whole at least once (a new inode): the
    // divergence check ran against protoledger's rewrite, not only appends.
    assert!(w2.changed.iter().any(|r| !r.followed), "no sync re-read the re-projected file whole: the check never measured it");
    common::once_each(&all.events[pre_len..], &wanted2, "turn 2 (after the re-projection)", |_| false);
    common::within_bound(&wanted2, &w2, "turn 2 (after the re-projection)");

    // The file Claude Code resumes from is one chain from turn 2's last line
    // through the re-projection to the root, without the error line; the
    // ledger still holds it, redacted.
    let end = std::fs::read(&loc).unwrap();
    assert!(end.starts_with(&after), "turn 2 did not append to the re-projected file");
    let full = chain(&end, "the resumed file");
    assert!(full.ends_with(&resumed) && !full.contains(&ERROR_LINE.to_string()));
    assert!(error_lines(&end).is_empty());
    let ids: HashSet<&str> = all.events.iter().map(|e| e.id.as_str()).collect();
    assert!(error.iter().all(|e| ids.contains(e.id.as_str())) && all.redacted().contains(&error[0].id));
    eprintln!(
        "sim: reproject: {} events in turn 1 (the error line's {} redacted), {} in turn 2; the resumed file is a chain of {} lines",
        wanted1.len(),
        error.len(),
        wanted2.len(),
        full.len()
    );

    paths.store(&s);
    paths.assert_isolated("claude-code reproject");
}
