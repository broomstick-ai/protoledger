//! Re-projection (SPEC §11.2): `sync::reproject` writes a projection
//! binding's location again from the projected view, and sync measures the
//! location against the latest `protoledger/projected` fact it matches, so a
//! rewrite protoledger made is never diverged while a foreign edit still is.
//!
//! Crashes are injected with [`sync::reproject_with`] / [`sync::project_with`]:
//! the hook returns an error at a [`Stage`], which stops the call there
//! exactly as a crash would (nothing after it runs, nothing is cleaned up).
//! Every test works in its own temporary directory on committed fixtures.
#![cfg(feature = "sync")]

use protoledger::formats::{self, ImportOptions};
use protoledger::store::Store;
use protoledger::store::bindings::{self, Projected, sha256_hex};
use protoledger::sync::{self, Stage};
use protoledger::{Event, Kind, Ledger};
use serde_json::Value;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

fn fixture(p: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(p)
}

const PLAIN_CHAT: &str = "claude-code/plain-chat/native/1ead304c-a7bc-443a-8455-a1778cc62693.jsonl";
const PLAIN_CHAT_SESSION: &str = "1ead304c-a7bc-443a-8455-a1778cc62693";
const API_ERROR: &str = "claude-code/api-error/native/aa076ad2-c0b7-41bb-b3be-5bd38a0bbbf2.jsonl";
const SUBAGENT: &str = "claude-code/subagent-sync/native/3955358c-cf2e-4dd2-a240-838affed0178.jsonl";
/// The answer of plain-chat, the event re-projection is asked to leave out.
const ANSWER: &str = "Hi! I'm ready to help";

fn ledger(s: &Store, agent: &str) -> Ledger {
    Ledger::read(&s.ledger_path(agent).unwrap()).unwrap()
}

fn facts(s: &Store, agent: &str, binding: &str) -> Vec<Projected> {
    bindings::projections(&ledger(s, agent).events, binding).facts.into_iter().map(|f| f.data).collect()
}

fn diverged(s: &Store, agent: &str) -> Vec<String> {
    ledger(s, agent).events.iter().filter(|e| e.str("type") == Some("protoledger/diverged")).map(|e| e.id.clone()).collect()
}

/// The ids the Claude Code importer reads for `agent` from `path`; panics
/// when the import does not hold the agent (so an absence check over it
/// cannot pass on an empty import).
fn import_ids(path: &Path, agent: &str) -> Vec<String> {
    let imp = formats::by_name("claude-code").unwrap().import(path, &ImportOptions::standard()).unwrap();
    let l = imp.set.get(agent).unwrap_or_else(|| panic!("{} does not import as {agent}", path.display()));
    l.events.iter().map(|e| e.id.clone()).collect()
}

fn append(path: &Path, bytes: &str) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(bytes.as_bytes()).unwrap();
    f.sync_all().unwrap();
}

/// Every temporary file of a re-projection under `dir` (any file whose name
/// holds `protoledger-tmp`, however it is spelled around that).
fn temps(dir: &Path) -> Vec<PathBuf> {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("protoledger-tmp"))
        .map(|e| e.into_path())
        .collect()
}

fn inode(p: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).unwrap().ino()
}

/// A crash at `at`: the hook for `reproject_with` / `project_with`.
fn crash_at(at: &'static str) -> impl FnMut(Stage<'_>) -> anyhow::Result<()> {
    move |st| {
        let name = match st {
            Stage::Written => "written",
            Stage::Bound => "bound",
            Stage::Synced { .. } => "synced",
            Stage::Rendered(_) => "rendered",
            Stage::TempsWritten => "temps",
            Stage::FactAppended => "fact",
            Stage::SidecarsRenamed => "sidecars",
            Stage::Renamed => "renamed",
            _ => "other",
        };
        if name == at { Err(anyhow::anyhow!("injected crash at {at}")) } else { Ok(()) }
    }
}

struct Bound {
    agent: String,
    binding: String,
    loc: PathBuf,
}

/// plain-chat imported into a store and projected into `<root>/cc`.
fn plain_chat(root: &Path) -> (Store, Bound) {
    projected(root, PLAIN_CHAT)
}

fn projected(root: &Path, fx: &str) -> (Store, Bound) {
    let mut s = Store::open(root.join("store")).unwrap();
    let imp = s.import("claude-code", &fixture(fx)).unwrap();
    let agent = imp.created[0].clone();
    let b = sync::project(&mut s, &agent, "claude-code", &root.join("cc")).unwrap();
    let loc = PathBuf::from(&b.location);
    (s, Bound { agent, binding: b.binding, loc })
}

/// The id of the event of `agent` whose text starts with `text`.
fn event_with_text(s: &Store, agent: &str, text: &str) -> Event {
    ledger(s, agent).events.into_iter().find(|e| e.str("text").is_some_and(|t| t.starts_with(text))).unwrap_or_else(|| panic!("no event says {text:?}"))
}

fn redact(s: &mut Store, agent: &str, id: &str) {
    s.redact(agent, &[id.to_string()], "test: taken out of the projection").unwrap();
}

/// Lines a harness resuming `session` appends after `parent`: plain-chat's
/// lines that have a uuid, each uuid made new (its first characters
/// replaced by `tag`) and each message id too, the first one's parent
/// pointed at `parent`.
fn harness_lines(session: &str, parent: &str, tag: &str) -> String {
    let text = std::fs::read_to_string(fixture(PLAIN_CHAT)).unwrap();
    let mut out = String::new();
    let mut first = true;
    for l in text.lines() {
        let mut v: Value = serde_json::from_str(l).unwrap();
        if v["uuid"].as_str().is_none() {
            continue;
        }
        v["sessionId"] = Value::from(session);
        let u = v["uuid"].as_str().unwrap().to_string();
        v["uuid"] = Value::from(format!("{tag}{}", &u[tag.len()..]));
        // A new turn's messages are new messages (their usage is keyed by message id).
        if let Some(m) = v["message"]["id"].as_str().map(str::to_string) {
            v["message"]["id"] = Value::from(format!("{m}_{tag}"));
        }
        if first {
            v["parentUuid"] = Value::from(parent);
            first = false;
        } else if let Some(p) = v["parentUuid"].as_str().map(str::to_string) {
            v["parentUuid"] = Value::from(format!("{tag}{}", &p[tag.len()..]));
        }
        out.push_str(&serde_json::to_string(&v).unwrap());
        out.push('\n');
    }
    out.replace(PLAIN_CHAT_SESSION, session)
}

/// The uuid of the file's last line that has one.
fn leaf(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| v["uuid"].as_str().map(str::to_string))
        .next_back()
        .unwrap()
}

fn session(b: &Bound) -> String {
    b.loc.file_stem().unwrap().to_string_lossy().into_owned()
}

// ---- the write, idempotence, the fact (step 6) ----

/// Re-projection leaves the redacted event out of the file while the ledger
/// keeps it; the file is replaced atomically (a new inode, no temporary file
/// left); the fact names the file's bytes and the view's last event. Done
/// again with nothing changed it writes nothing and appends nothing (the
/// same bytes), so there is one `protoledger/projected` fact per change:
/// project's, the redaction's, and the unredaction's.
#[test]
fn reproject_writes_the_projected_view_atomically_and_is_idempotent_one_fact_per_change() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, b) = plain_chat(dir.path());
    // `project` appended its fact right after `bound`, naming the file it wrote.
    let first = facts(&s, &b.agent, &b.binding);
    assert_eq!(first.len(), 1, "project appends one protoledger/projected fact");
    let written = std::fs::read(&b.loc).unwrap();
    assert_eq!((first[0].bytes, first[0].sha256.clone()), (written.len() as u64, sha256_hex(&written)));
    assert!(!bindings::projections(&ledger(&s, &b.agent).events, &b.binding).legacy);

    let answer = event_with_text(&s, &b.agent, ANSWER);
    assert!(import_ids(&b.loc, &b.agent).contains(&answer.id), "sentinel: the answer is in the file before");
    redact(&mut s, &b.agent, &answer.id);
    let ino = inode(&b.loc);
    let r = sync::reproject(&mut s, &b.binding).unwrap();
    assert_eq!(r.written, vec![b.loc.clone()]);
    assert!(r.fact.is_some());
    assert_ne!(inode(&b.loc), ino, "the file was replaced by a rename (a new inode), not written in place");
    assert!(temps(dir.path()).is_empty(), "a temporary file was left: {:?}", temps(dir.path()));
    let file = std::fs::read(&b.loc).unwrap();
    assert!(!import_ids(&b.loc, &b.agent).contains(&answer.id), "the redacted event is still in the file");
    assert!(!String::from_utf8_lossy(&file).contains(ANSWER));
    let l = ledger(&s, &b.agent);
    assert_eq!(l.events.iter().find(|e| e.id == answer.id), Some(&answer), "the ledger keeps the redacted event unchanged");
    // The fact: bytes, sha256, through.
    let view_last = l.projected_view().last().map(|e| e.id.clone());
    assert!(view_last.is_some() && view_last.as_deref() != Some(answer.id.as_str()));
    assert_eq!(r.projected, Projected { binding: b.binding.clone(), bytes: file.len() as u64, sha256: sha256_hex(&file), through: view_last });
    assert_eq!(facts(&s, &b.agent, &b.binding).last(), Some(&r.projected));

    // Idempotent: the same bytes, nothing written, no fact.
    let n = l.events.len();
    for _ in 0..3 {
        let again = sync::reproject(&mut s, &b.binding).unwrap();
        assert!(again.written.is_empty() && again.fact.is_none(), "an unchanged view was written again: {:?} {:?}", again.written, again.fact);
        assert_eq!(std::fs::read(&b.loc).unwrap(), file, "the same bytes");
    }
    assert_eq!(ledger(&s, &b.agent).events.len(), n, "an unchanged view appended something");
    assert_eq!(facts(&s, &b.agent, &b.binding).len(), 2, "one fact per change: project's and the redaction's");

    // Unredacting is a change: one more fact, and the answer is back.
    s.unredact(&b.agent, std::slice::from_ref(&answer.id)).unwrap();
    let back = sync::reproject(&mut s, &b.binding).unwrap();
    assert!(back.fact.is_some() && import_ids(&b.loc, &b.agent).contains(&answer.id));
    assert_eq!(facts(&s, &b.agent, &b.binding).len(), 3);
    assert!(diverged(&s, &b.agent).is_empty());
    // And a sync after all of it finds nothing to do.
    let idle = sync::sync(&mut s, &b.binding).unwrap();
    assert!(!idle.changed(), "{idle:?}");
}

/// `through` is `null` when the bound agent's projected view holds no
/// event (every event redacted, the binding's own `bound` fact included).
#[test]
fn through_is_null_for_an_empty_view() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path().join("store")).unwrap();
    s.import("protoledger", &fixture("protoledger/minimal/native")).unwrap();
    let b = sync::project(&mut s, "hello-1", "claude-code", &dir.path().join("cc")).unwrap();
    let l = ledger(&s, "hello-1");
    assert!(!l.projected_view().is_empty(), "sentinel: the view holds events before");
    let all: Vec<String> = l.events.iter().filter(|e| protoledger::ledger::fact_type(e).is_none()).map(|e| e.id.clone()).collect();
    s.redact("hello-1", &all, "test: an empty view").unwrap();
    assert!(ledger(&s, "hello-1").projected_view().is_empty());
    let r = sync::reproject(&mut s, &b.binding).unwrap();
    assert_eq!(r.projected.through, None);
    let raw = ledger(&s, "hello-1").events.iter().rev().find(|e| e.str("type") == Some("protoledger/projected")).unwrap().body["data"]["through"].clone();
    assert_eq!(raw, Value::Null, "through is written as null");
}

// ---- refusals (steps 1, 2 and 3) ----

/// Step 1: an inactive binding (unbound, diverged), an attachment and a
/// location that is not a single append-only file (opencode) are refused,
/// appending nothing and leaving the location as it is. Opposed by the
/// active Claude Code projection the other tests re-project.
#[test]
fn reproject_refuses_an_inactive_binding_an_attachment_and_a_store_that_is_not_one_append_only_file() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, b) = plain_chat(dir.path());
    let check = |s: &mut Store, binding: &str, agent: &str, loc: &Path, why: &str| {
        let before = std::fs::read(loc).ok();
        let n = ledger(s, agent).events.len();
        let e = sync::reproject(s, binding).expect_err(why);
        assert!(format!("{e:#}").contains(why), "{e:#}");
        assert_eq!(std::fs::read(loc).ok(), before, "{why}: the location changed");
        assert_eq!(ledger(s, agent).events.len(), n, "{why}: something was appended");
    };
    // Unbound.
    sync::unbind(&mut s, &b.binding).unwrap();
    check(&mut s, &b.binding, &b.agent, &b.loc, "not active (unbound)");

    // An attachment.
    let native = dir.path().join("native");
    std::fs::create_dir_all(&native).unwrap();
    let copy = native.join(format!("{PLAIN_CHAT_SESSION}.jsonl"));
    std::fs::copy(fixture(PLAIN_CHAT), &copy).unwrap();
    let mut s2 = Store::open(dir.path().join("store2")).unwrap();
    let (a, _) = sync::attach(&mut s2, "claude-code", &copy).unwrap();
    check(&mut s2, &a.binding, &a.agent, &copy, "is an attachment");

    // opencode: its projection is a store the harness rewrites, never one file.
    let mut s3 = Store::open(dir.path().join("store3")).unwrap();
    s3.import("protoledger", &fixture("protoledger/minimal/native")).unwrap();
    let o = sync::project(&mut s3, "hello-1", "opencode", &dir.path().join("oc")).unwrap();
    assert!(facts(&s3, "hello-1", &o.binding).is_empty(), "a binding re-projection refuses gets no projected fact");
    check(&mut s3, &o.binding, "hello-1", Path::new(&o.location), "not a single file its harness only appends to");

    // Diverged: a line taken out of the middle by someone else.
    let dir2 = tempfile::tempdir().unwrap();
    let (mut s4, d) = plain_chat(dir2.path());
    let text = std::fs::read_to_string(&d.loc).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    let cut: String = lines.iter().enumerate().filter(|(i, _)| *i != 3).map(|(_, l)| format!("{l}\n")).collect();
    std::fs::write(&d.loc, cut).unwrap();
    assert!(sync::sync(&mut s4, &d.binding).unwrap().diverged.is_some());
    check(&mut s4, &d.binding, &d.agent, &d.loc, "not active (diverged)");
}

/// Step 2: re-projection syncs first, so what the harness appended reaches the
/// ledger with no explicit sync and is in the file it writes; a foreign edit
/// found by that sync refuses it and leaves the file byte for byte.
#[test]
fn reproject_syncs_first_and_a_foreign_edit_refuses_it_leaving_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, b) = plain_chat(dir.path());
    let lines = harness_lines(&session(&b), &leaf(&b.loc), "aaaa");
    append(&b.loc, &lines);
    let wanted: Vec<String> = import_ids(&b.loc, &b.agent).into_iter().filter(|id| id.starts_with("aaaa")).collect();
    assert!(wanted.len() >= 2, "sentinel: the appended lines make events: {wanted:?}");
    let held: HashSet<String> = ledger(&s, &b.agent).events.into_iter().map(|e| e.id).collect();
    assert!(wanted.iter().all(|id| !held.contains(id)), "sentinel: not in the ledger before");
    let answer = event_with_text(&s, &b.agent, ANSWER);
    redact(&mut s, &b.agent, &answer.id);
    let r = sync::reproject(&mut s, &b.binding).unwrap();
    let got: Vec<String> = r.synced.appended.iter().map(|p| p.event.id.clone()).collect();
    assert!(wanted.iter().all(|id| got.contains(id)), "the pre-sync did not ingest {wanted:?}: {got:?}");
    let now = import_ids(&b.loc, &b.agent);
    assert!(wanted.iter().all(|id| now.contains(id)), "the harness's lines are not in the re-projected file");

    // A foreign edit: a line of protoledger's render removed.
    let text = std::fs::read_to_string(&b.loc).unwrap();
    let cut: String = text.lines().enumerate().filter(|(i, _)| *i != 4).map(|(_, l)| format!("{l}\n")).collect();
    std::fs::write(&b.loc, &cut).unwrap();
    let n = facts(&s, &b.agent, &b.binding).len();
    let e = sync::reproject(&mut s, &b.binding).unwrap_err();
    assert!(format!("{e:#}").contains("diverged"), "{e:#}");
    assert_eq!(std::fs::read_to_string(&b.loc).unwrap(), cut, "a refused re-projection changed the file");
    assert_eq!(facts(&s, &b.agent, &b.binding).len(), n, "a refused re-projection appended a fact");
    assert_eq!(diverged(&s, &b.agent).len(), 1);
}

/// Step 3: a render that does not import back as itself (here, a stranger line
/// injected into it) is refused before anything is written: the file, the
/// ledger and the directory are as they were. Opposed: without the stranger
/// the same re-projection writes.
#[test]
fn a_render_that_does_not_import_back_as_itself_is_refused_writing_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, b) = plain_chat(dir.path());
    let answer = event_with_text(&s, &b.agent, ANSWER);
    redact(&mut s, &b.agent, &answer.id);
    let before = std::fs::read(&b.loc).unwrap();
    let n = ledger(&s, &b.agent).events.len();
    let stranger = harness_lines(&session(&b), &leaf(&b.loc), "bbbb");
    let first = stranger.lines().next().unwrap().to_string();
    let rel = b.loc.file_name().unwrap().to_string_lossy().into_owned();
    let mut inject = |st: Stage<'_>| {
        if let Stage::Rendered(out) = st {
            let f = out.files.get_mut(&rel).expect("the render holds the location file");
            f.extend_from_slice(format!("{first}\n").as_bytes());
        }
        Ok(())
    };
    let e = sync::reproject_with(&mut s, &b.binding, &mut inject).unwrap_err();
    assert!(format!("{e:#}").contains("does not import back as itself"), "{e:#}");
    assert_eq!(std::fs::read(&b.loc).unwrap(), before, "a refused render was written");
    assert_eq!(ledger(&s, &b.agent).events.len(), n, "a refused render appended something");
    assert!(temps(dir.path()).is_empty());
    let ok = sync::reproject(&mut s, &b.binding).unwrap();
    assert_eq!(ok.written, vec![b.loc.clone()], "opposed: the same re-projection without the stranger writes");
}

// ---- a line the harness writes after the sync (step 4) ----

/// A line the harness appends after step 2's sync (injected once, just
/// before the step-4 check) is never lost: the check sees the file changed,
/// re-projection goes back to step 2, whose sync ingests the line, and the
/// file it then writes holds it.
#[test]
fn a_harness_line_written_after_the_sync_is_ingested_and_kept_never_lost() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, b) = plain_chat(dir.path());
    let answer = event_with_text(&s, &b.agent, ANSWER);
    redact(&mut s, &b.agent, &answer.id);
    let line = harness_lines(&session(&b), &leaf(&b.loc), "cccc");
    let loc = b.loc.clone();
    let mut once = Some(line);
    let mut hook = |st: Stage<'_>| {
        if matches!(st, Stage::TempsWritten)
            && let Some(l) = once.take()
        {
            append(&loc, &l);
        }
        Ok(())
    };
    let r = sync::reproject_with(&mut s, &b.binding, &mut hook).unwrap();
    assert!(once.is_none(), "sentinel: the line was written");
    let in_ledger: Vec<String> = ledger(&s, &b.agent).events.into_iter().map(|e| e.id).filter(|id| id.starts_with("cccc")).collect();
    let in_file: Vec<String> = import_ids(&b.loc, &b.agent).into_iter().filter(|id| id.starts_with("cccc")).collect();
    assert!(!in_ledger.is_empty() || !in_file.is_empty(), "the harness's line is lost: in neither the ledger nor the file");
    assert!(!in_ledger.is_empty(), "the harness's line is not in the ledger");
    assert_eq!(in_file, in_ledger, "the harness's line is not in the file");
    assert_eq!(r.attempts, 2, "the write after the sync was not seen");
    assert!(!import_ids(&b.loc, &b.agent).contains(&answer.id));
}

/// Step 4's "or refuses": a harness that writes the location during every
/// attempt (after each step-2 sync, before the step-4 check) makes
/// re-projection give up after `ATTEMPTS`: refused, nothing written by
/// protoledger (the file is the harness's own, same inode, every line it
/// wrote in it and in the ledger), no fact appended, no temporary file left.
#[test]
fn a_harness_writing_during_every_attempt_is_refused_after_the_attempts_writing_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, b) = plain_chat(dir.path());
    let answer = event_with_text(&s, &b.agent, ANSWER);
    redact(&mut s, &b.agent, &answer.id);
    let ino = inode(&b.loc);
    let n_facts = facts(&s, &b.agent, &b.binding).len();
    let loc = b.loc.clone();
    let sess = session(&b);
    let tags = ["c1c1", "c2c2", "c3c3", "c4c4", "c5c5"];
    let mut writes = 0usize;
    let mut hook = |st: Stage<'_>| {
        if matches!(st, Stage::TempsWritten) {
            append(&loc, &harness_lines(&sess, &leaf(&loc), tags[writes]));
            writes += 1;
        }
        Ok(())
    };
    let e = sync::reproject_with(&mut s, &b.binding, &mut hook).unwrap_err();
    assert!(format!("{e:#}").contains(&format!("changed during each of {} attempts", sync::ATTEMPTS)), "{e:#}");
    assert_eq!(writes, sync::ATTEMPTS as usize, "one harness write per attempt");
    assert_eq!(inode(&b.loc), ino, "the file was replaced");
    assert!(import_ids(&b.loc, &b.agent).contains(&answer.id), "protoledger's render was written");
    assert_eq!(facts(&s, &b.agent, &b.binding).len(), n_facts, "a refused re-projection appended a fact");
    assert!(temps(dir.path()).is_empty(), "left behind: {:?}", temps(dir.path()));
    // What the harness wrote is in the file and, up to the last attempt's
    // sync, in the ledger; the next sync takes the rest, and nothing diverged.
    let r = sync::sync(&mut s, &b.binding).unwrap();
    assert!(r.diverged.is_none(), "{r:?}");
    let held: HashSet<String> = ledger(&s, &b.agent).events.into_iter().map(|e| e.id).collect();
    let in_file = import_ids(&b.loc, &b.agent);
    for t in &tags[..writes] {
        let ids: Vec<&String> = in_file.iter().filter(|id| id.starts_with(t)).collect();
        assert!(!ids.is_empty() && ids.iter().all(|id| held.contains(*id)), "{t}: the harness's lines are not in the file and the ledger");
    }
}

// ---- crashes (after the fact, twice, after the rename, temp files) ----

/// A crash after the fact is appended and before the rename leaves the
/// old file, which an older fact matches: sync finds nothing diverged, and
/// the next re-projection writes the file the latest fact names and appends
/// no new fact.
#[test]
fn a_crash_after_the_fact_before_the_rename_is_not_diverged_and_the_next_reprojection_writes_without_a_fact() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, b) = plain_chat(dir.path());
    let old = std::fs::read(&b.loc).unwrap();
    let answer = event_with_text(&s, &b.agent, ANSWER);
    redact(&mut s, &b.agent, &answer.id);
    sync::reproject_with(&mut s, &b.binding, &mut crash_at("fact")).unwrap_err();
    assert_eq!(std::fs::read(&b.loc).unwrap(), old, "the rename happened");
    let fs = facts(&s, &b.agent, &b.binding);
    assert_eq!(fs.len(), 2);
    assert!(!fs[1].matches(&old), "sentinel: the latest fact does not match the file left in place");
    let r = sync::sync(&mut s, &b.binding).unwrap();
    assert!(r.diverged.is_none() && !r.changed(), "{r:?}");
    let next = sync::reproject(&mut s, &b.binding).unwrap();
    assert_eq!(next.written, vec![b.loc.clone()]);
    assert!(next.fact.is_none(), "a second fact for the same render");
    let file = std::fs::read(&b.loc).unwrap();
    assert!(fs[1].matches(&file) && fs[1].bytes == file.len() as u64, "the file is not the one the latest fact names");
    assert!(sync::sync(&mut s, &b.binding).unwrap().diverged.is_none());
    assert!(temps(dir.path()).is_empty());
}

/// Two crashes in a row before the rename, each after a fact for a
/// different render: the file is still the one `project` wrote, which only
/// the oldest fact matches. Not diverged; the next re-projection writes the
/// newest render with no new fact.
#[test]
fn two_consecutive_crashes_before_the_rename_are_not_diverged() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, b) = plain_chat(dir.path());
    let old = std::fs::read(&b.loc).unwrap();
    let answer = event_with_text(&s, &b.agent, ANSWER);
    redact(&mut s, &b.agent, &answer.id);
    sync::reproject_with(&mut s, &b.binding, &mut crash_at("fact")).unwrap_err();
    let prompt = event_with_text(&s, &b.agent, "Say hello");
    redact(&mut s, &b.agent, &prompt.id);
    sync::reproject_with(&mut s, &b.binding, &mut crash_at("fact")).unwrap_err();
    let fs = facts(&s, &b.agent, &b.binding);
    assert_eq!(fs.len(), 3, "three facts: project's and one per crashed re-projection");
    assert_eq!(std::fs::read(&b.loc).unwrap(), old);
    assert!(!fs[1].matches(&old) && !fs[2].matches(&old) && fs[0].matches(&old), "sentinel: only project's fact matches");
    let r = sync::sync(&mut s, &b.binding).unwrap();
    assert!(r.diverged.is_none(), "{r:?}");
    let next = sync::reproject(&mut s, &b.binding).unwrap();
    assert!(next.fact.is_none() && fs[2].matches(&std::fs::read(&b.loc).unwrap()));
    assert!(diverged(&s, &b.agent).is_empty());
}

/// A crash after the rename and before the cursor is re-baselined (step 7):
/// the cursor still holds what the pre-sync read, the redacted event
/// included, but the file matches the latest fact: not diverged.
#[test]
fn a_crash_after_the_rename_before_the_cursor_is_rebaselined_is_not_diverged() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, b) = plain_chat(dir.path());
    let answer = event_with_text(&s, &b.agent, ANSWER);
    redact(&mut s, &b.agent, &answer.id);
    sync::reproject_with(&mut s, &b.binding, &mut crash_at("renamed")).unwrap_err();
    let cursor = s.cursor(&b.binding).unwrap().unwrap();
    assert!(cursor.contains(&answer.id), "sentinel: the cursor was not re-baselined (it still names the redacted event)");
    assert!(!import_ids(&b.loc, &b.agent).contains(&answer.id), "sentinel: the rename happened");
    let r = sync::sync(&mut s, &b.binding).unwrap();
    assert!(r.diverged.is_none() && r.appended.is_empty(), "{r:?}");
    // And a turn after it comes in once.
    append(&b.loc, &harness_lines(&session(&b), &leaf(&b.loc), "dddd"));
    let r = sync::sync(&mut s, &b.binding).unwrap();
    assert!(r.diverged.is_none() && r.appended.iter().any(|p| p.event.id.starts_with("dddd")), "{r:?}");
}

/// A temporary file a crash leaves (after step 4's write) is ignored by
/// sync, which neither defers, diverges nor appends, and is deleted by the
/// next re-projection of the binding, even one that writes nothing.
#[test]
fn a_leftover_temp_file_is_ignored_by_sync_and_deleted_by_the_next_reprojection() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, b) = projected(dir.path(), SUBAGENT);
    let side = b.loc.with_extension("");
    assert!(side.is_dir(), "sentinel: the projection has a sidecar directory");
    // A first re-projection (it carries the binding's own `bound` fact), so
    // that withdrawing the redactions below leaves nothing to write.
    sync::reproject(&mut s, &b.binding).unwrap();
    let answer = ledger(&s, &b.agent).events.into_iter().rev().find(|e| e.kind == Kind::Text && e.str("role") == Some("assistant")).unwrap();
    let sub = s.children(&b.agent).unwrap()[0].id.clone();
    let sub_answer = ledger(&s, &sub).events.into_iter().rev().find(|e| e.kind == Kind::Text && e.str("role") == Some("assistant")).unwrap();
    redact(&mut s, &b.agent, &answer.id);
    redact(&mut s, &sub, &sub_answer.id);
    let before = std::fs::read(&b.loc).unwrap();
    sync::reproject_with(&mut s, &b.binding, &mut crash_at("temps")).unwrap_err();
    let left = temps(dir.path());
    assert!(left.len() >= 2 && left.iter().any(|t| t.starts_with(&side)), "sentinel: a temp beside the file and one in the sidecar: {left:?}");
    // A torn temp, as a crash mid-write leaves one.
    let torn = left.iter().find(|t| t.starts_with(&side)).unwrap();
    std::fs::write(torn, b"{\"type\":\"user\",\"uuid\":\"torn").unwrap();
    let r = sync::sync(&mut s, &b.binding).unwrap();
    assert!(r.deferred.is_none() && r.diverged.is_none() && r.appended.is_empty() && r.created.is_empty(), "{r:?}");
    assert_eq!(std::fs::read(&b.loc).unwrap(), before);
    // Withdraw both redactions: the next re-projection has nothing to write,
    // and still deletes what the crash left.
    s.unredact(&b.agent, std::slice::from_ref(&answer.id)).unwrap();
    s.unredact(&sub, std::slice::from_ref(&sub_answer.id)).unwrap();
    let next = sync::reproject(&mut s, &b.binding).unwrap();
    assert!(next.written.is_empty(), "sentinel: this re-projection writes nothing: {:?}", next.written);
    let mut removed = next.removed_temps.clone();
    removed.sort();
    let mut want = left.clone();
    want.sort();
    assert_eq!(removed, want);
    assert!(temps(dir.path()).is_empty(), "left behind: {:?}", temps(dir.path()));
}

// ---- the cursor lost ----

/// The facts are in the ledger, so with the cursor lost (index.db
/// deleted between the re-projection and the sync, here after a crash
/// before the cursor was re-baselined) protoledger's rewrite is still not
/// diverged, and a turn after it comes in once. Opposed: with the cursor
/// lost, a foreign edit of protoledger's bytes is still diverged.
#[test]
fn with_the_cursor_lost_the_rewrite_is_not_diverged_and_a_foreign_edit_still_is() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, b) = plain_chat(dir.path());
    let answer = event_with_text(&s, &b.agent, ANSWER);
    redact(&mut s, &b.agent, &answer.id);
    sync::reproject_with(&mut s, &b.binding, &mut crash_at("renamed")).unwrap_err();
    drop(s);
    std::fs::remove_file(dir.path().join("store/index.db")).unwrap();
    let mut s = Store::open(dir.path().join("store")).unwrap();
    assert!(s.cursor(&b.binding).unwrap().is_none(), "sentinel: the cursor is lost");
    let r = sync::sync(&mut s, &b.binding).unwrap();
    assert!(r.diverged.is_none() && r.appended.is_empty(), "{r:?}");
    append(&b.loc, &harness_lines(&session(&b), &leaf(&b.loc), "eeee"));
    let r = sync::sync(&mut s, &b.binding).unwrap();
    let got: Vec<&str> = r.appended.iter().map(|p| p.event.id.as_str()).filter(|id| id.starts_with("eeee")).collect();
    assert!(r.diverged.is_none() && !got.is_empty(), "{r:?}");
    let ids: Vec<String> = ledger(&s, &b.agent).events.into_iter().map(|e| e.id).collect();
    assert!(got.iter().all(|g| ids.iter().filter(|i| i == g).count() == 1));

    // A foreign edit of protoledger's bytes, the cursor lost again: one
    // character of a line's text changed (ids unchanged).
    drop(s);
    std::fs::remove_file(dir.path().join("store/index.db")).unwrap();
    let mut s = Store::open(dir.path().join("store")).unwrap();
    let text = std::fs::read_to_string(&b.loc).unwrap();
    assert!(text.contains("Say hello in one short sentence."));
    std::fs::write(&b.loc, text.replacen("Say hello in one short sentence.", "Say hullo in one short sentence.", 1)).unwrap();
    let r = sync::sync(&mut s, &b.binding).unwrap();
    assert!(r.diverged.as_deref().is_some_and(|d| d.contains("no protoledger/projected fact")), "{r:?}");
}

// ---- foreign edits ----

/// After a re-projection the harness appends a turn and a sync ingests
/// it; a foreign program truncating the file back to the fact's bytes is
/// diverged (§9.3 beyond the bytes). A foreign edit inside protoledger's
/// bytes that keeps every id (one character of text) is diverged too: no
/// fact matches.
#[test]
fn a_foreign_edit_after_reprojection_is_diverged_truncation_and_an_edit_keeping_ids() {
    for case in ["truncate", "edit"] {
        let dir = tempfile::tempdir().unwrap();
        let (mut s, b) = plain_chat(dir.path());
        let answer = event_with_text(&s, &b.agent, ANSWER);
        redact(&mut s, &b.agent, &answer.id);
        let r = sync::reproject(&mut s, &b.binding).unwrap();
        let projected = std::fs::read(&b.loc).unwrap();
        append(&b.loc, &harness_lines(&session(&b), &leaf(&b.loc), "ffff"));
        let ingested = sync::sync(&mut s, &b.binding).unwrap();
        assert!(ingested.appended.iter().any(|p| p.event.id.starts_with("ffff")) && ingested.diverged.is_none());
        match case {
            "truncate" => std::fs::write(&b.loc, &projected).unwrap(),
            _ => {
                let text = std::fs::read_to_string(&b.loc).unwrap();
                assert!(text.contains("Say hello in one short sentence."));
                std::fs::write(&b.loc, text.replacen("Say hello in one short sentence.", "Say hullo in one short sentence.", 1)).unwrap();
                let now = import_ids(&b.loc, &b.agent);
                let held: HashSet<String> = ledger(&s, &b.agent).events.into_iter().map(|e| e.id).collect();
                assert!(now.iter().all(|id| held.contains(id)), "sentinel: the edit keeps every id (§9.3 alone cannot see it)");
            }
        }
        assert_eq!(r.projected.bytes, projected.len() as u64);
        let d = sync::sync(&mut s, &b.binding).unwrap();
        assert!(d.diverged.is_some(), "{case}: a foreign edit was not diverged: {d:?}");
        assert_eq!(diverged(&s, &b.agent).len(), 1, "{case}");
    }
}

/// A file put back to an older projection after a turn a sync matching that
/// projection read is diverged; an exact restore with no turn read after it
/// is not (every byte is protoledger's render), and the next re-projection
/// rewrites it with no new fact.
#[test]
fn an_older_projection_put_back_is_diverged_only_after_a_turn_was_read_past_it() {
    for read_a_turn in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let (mut s, b) = plain_chat(dir.path());
        let answer = event_with_text(&s, &b.agent, ANSWER);
        redact(&mut s, &b.agent, &answer.id);
        sync::reproject(&mut s, &b.binding).unwrap();
        let p1 = std::fs::read(&b.loc).unwrap();
        if read_a_turn {
            append(&b.loc, &harness_lines(&session(&b), &leaf(&b.loc), "9999"));
            assert!(!sync::sync(&mut s, &b.binding).unwrap().appended.is_empty());
        }
        let prompt = event_with_text(&s, &b.agent, "Say hello");
        redact(&mut s, &b.agent, &prompt.id);
        sync::reproject(&mut s, &b.binding).unwrap();
        let p2 = std::fs::read(&b.loc).unwrap();
        assert_ne!(p1, p2);
        std::fs::write(&b.loc, &p1).unwrap();
        let r = sync::sync(&mut s, &b.binding).unwrap();
        if read_a_turn {
            assert!(r.diverged.as_deref().is_some_and(|d| d.contains("read past the projected bytes are gone")), "{r:?}");
        } else {
            assert!(r.diverged.is_none(), "an exact restore with no turn read was diverged: {r:?}");
            let n = facts(&s, &b.agent, &b.binding).len();
            let again = sync::reproject(&mut s, &b.binding).unwrap();
            assert!(again.fact.is_none() && facts(&s, &b.agent, &b.binding).len() == n);
            assert_eq!(std::fs::read(&b.loc).unwrap(), p2, "the next re-projection rewrites it");
        }
    }
}

/// A diverged projection binding stays stopped: re-projection refuses it,
/// before and after `unbind`; the way out is a new projection to a new
/// location, which re-projects.
#[test]
fn a_diverged_projection_is_refused_and_the_way_out_is_unbind_and_a_new_projection() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, b) = plain_chat(dir.path());
    let text = std::fs::read_to_string(&b.loc).unwrap();
    std::fs::write(&b.loc, text.replacen("Say hello in one short sentence.", "Say hullo in one short sentence.", 1)).unwrap();
    assert!(sync::sync(&mut s, &b.binding).unwrap().diverged.is_some());
    let e = sync::reproject(&mut s, &b.binding).unwrap_err();
    assert!(format!("{e:#}").contains("diverged"), "{e:#}");
    assert!(sync::unbind(&mut s, &b.binding).is_err(), "a diverged binding is already not active");
    let fresh = sync::project(&mut s, &b.agent, "claude-code", &dir.path().join("cc2")).unwrap();
    let answer = event_with_text(&s, &b.agent, ANSWER);
    redact(&mut s, &b.agent, &answer.id);
    let r = sync::reproject(&mut s, &fresh.binding).unwrap();
    assert_eq!(r.written, vec![PathBuf::from(&fresh.location)]);
    assert!(std::fs::read_to_string(&b.loc).unwrap().contains("Say hullo"), "the diverged location was written");
}

// ---- legacy bindings ----

/// A binding made before SPEC §11.2 (no `protoledger/projected` fact after
/// its `bound` fact, as `project` wrote them, or as a `project` that stopped
/// between its two appends leaves one) is measured by §9.3 until a sync
/// matches one of its facts: a re-projection that crashed after its fact and
/// before its rename leaves the old file, which no fact matches, and that is
/// not diverged. Once a sync has matched a fact, a file none matches is.
#[test]
fn a_legacy_binding_is_measured_by_section_9_3_until_a_sync_matches_a_fact() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path().join("store")).unwrap();
    let imp = s.import("claude-code", &fixture(PLAIN_CHAT)).unwrap();
    let agent = imp.created[0].clone();
    // A crash between `bound` and `projected` leaves a legacy binding.
    let e = sync::project_with(&mut s, &agent, "claude-code", &dir.path().join("cc"), &mut crash_at("bound")).unwrap_err();
    assert!(format!("{e:#}").contains("injected"));
    let row = s.bindings().unwrap().pop().expect("the binding was bound");
    let p = bindings::projections(&ledger(&s, &agent).events, &row.binding);
    assert!(p.legacy && p.facts.is_empty(), "{p:?}");
    let b = Bound { agent: agent.clone(), binding: row.binding.clone(), loc: PathBuf::from(&row.location) };
    let old = std::fs::read(&b.loc).unwrap();
    // A turn comes in as before.
    append(&b.loc, &harness_lines(&session(&b), &leaf(&b.loc), "1111"));
    let r = sync::sync(&mut s, &b.binding).unwrap();
    assert!(r.diverged.is_none() && r.appended.iter().any(|p| p.event.id.starts_with("1111")), "{r:?}");
    // Its first re-projection crashes after the fact, before the rename.
    let answer = event_with_text(&s, &agent, ANSWER);
    redact(&mut s, &agent, &answer.id);
    sync::reproject_with(&mut s, &b.binding, &mut crash_at("fact")).unwrap_err();
    let fs = facts(&s, &agent, &b.binding);
    let file = std::fs::read(&b.loc).unwrap();
    assert!(file.starts_with(&old) && fs.len() == 1 && !fs[0].matches(&file), "sentinel: no fact matches the file left in place");
    let r = sync::sync(&mut s, &b.binding).unwrap();
    assert!(r.diverged.is_none(), "a legacy binding no fact matches was diverged before any sync matched one: {r:?}");
    // The next re-projection completes; a sync matches its fact.
    sync::reproject(&mut s, &b.binding).unwrap();
    assert!(sync::sync(&mut s, &b.binding).unwrap().diverged.is_none());
    // From now on a file no fact matches is diverged, legacy or not.
    let text = std::fs::read_to_string(&b.loc).unwrap();
    std::fs::write(&b.loc, text.replacen("Say hello in one short sentence.", "Say hullo in one short sentence.", 1)).unwrap();
    let r = sync::sync(&mut s, &b.binding).unwrap();
    assert!(r.diverged.is_some(), "{r:?}");
}

// ---- project's facts only after the file is durable ----

/// `project` appends `bound` and `projected` only after the files are
/// written: a crash between the write and the facts leaves no binding at
/// all (an unbound file), never a binding naming bytes that are not on disk.
#[test]
fn project_appends_its_facts_only_after_the_file_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path().join("store")).unwrap();
    let imp = s.import("claude-code", &fixture(PLAIN_CHAT)).unwrap();
    let agent = imp.created[0].clone();
    let n = ledger(&s, &agent).events.len();
    let cc = dir.path().join("cc");
    sync::project_with(&mut s, &agent, "claude-code", &cc, &mut crash_at("written")).unwrap_err();
    assert!(s.bindings().unwrap().is_empty(), "a binding was made before its file was durable");
    assert_eq!(ledger(&s, &agent).events.len(), n, "a fact was appended before the file was durable");
    let written: Vec<PathBuf> =
        walkdir::WalkDir::new(&cc).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()).map(|e| e.into_path()).collect();
    assert_eq!(written.len(), 1, "sentinel: the file was written: {written:?}");
    // Every binding the store holds names a file some fact (or §9.3) accepts.
    let b = sync::project(&mut s, &agent, "claude-code", &dir.path().join("cc2")).unwrap();
    let fs = facts(&s, &agent, &b.binding);
    assert!(fs.len() == 1 && fs[0].matches(&std::fs::read(&b.location).unwrap()));
}

// ---- sidecar files ----

/// A location's other files (Claude Code's sidecar directory) are
/// covered by no fact's bytes; their events are read past the bytes, so
/// §9.3 applies to them per fact, redacted events excepted. A crash between
/// the sidecar's rename and the location file's rename leaves the new
/// sidecar beside the old file: not diverged. A foreign edit of a sidecar
/// file (a line of a subagent removed) is.
#[test]
fn sidecar_files_are_measured_past_the_bytes_a_crash_between_renames_is_not_diverged_a_foreign_edit_is() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, b) = projected(dir.path(), SUBAGENT);
    let sub = s.children(&b.agent).unwrap()[0].id.clone();
    let side_file = walkdir::WalkDir::new(b.loc.with_extension(""))
        .into_iter()
        .filter_map(|e| e.ok())
        .find(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
        .unwrap()
        .into_path();
    let answer = ledger(&s, &b.agent).events.into_iter().rev().find(|e| e.kind == Kind::Text && e.str("role") == Some("assistant")).unwrap();
    let sub_answer = ledger(&s, &sub).events.into_iter().rev().find(|e| e.kind == Kind::Text && e.str("role") == Some("assistant")).unwrap();
    redact(&mut s, &b.agent, &answer.id);
    redact(&mut s, &sub, &sub_answer.id);
    let old_main = std::fs::read(&b.loc).unwrap();
    let old_side = std::fs::read(&side_file).unwrap();
    sync::reproject_with(&mut s, &b.binding, &mut crash_at("sidecars")).unwrap_err();
    assert_eq!(std::fs::read(&b.loc).unwrap(), old_main, "sentinel: the location file is the old one");
    assert_ne!(std::fs::read(&side_file).unwrap(), old_side, "sentinel: the sidecar file is the new one");
    assert!(!import_ids(&b.loc, &sub).contains(&sub_answer.id), "sentinel: the redacted subagent event left the sidecar");
    let r = sync::sync(&mut s, &b.binding).unwrap();
    assert!(r.diverged.is_none() && r.appended.is_empty(), "{r:?}");
    let done = sync::reproject(&mut s, &b.binding).unwrap();
    assert!(done.fact.is_none() && done.written == vec![b.loc.clone()], "{:?}", done.written);
    assert!(sync::sync(&mut s, &b.binding).unwrap().diverged.is_none());

    // A foreign edit of the sidecar: a subagent line removed (a non-redacted one).
    let text = std::fs::read_to_string(&side_file).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    let keep: String = lines.iter().enumerate().filter(|(i, _)| *i != 1).map(|(_, l)| format!("{l}\n")).collect();
    std::fs::write(&side_file, keep).unwrap();
    let r = sync::sync(&mut s, &b.binding).unwrap();
    assert!(r.diverged.as_deref().is_some_and(|d| d.contains(&sub)), "a foreign edit of a sidecar file was not diverged: {r:?}");
}

// ---- the new render a byte-prefix of the old file ----

/// The new render is a byte-prefix of the old file (only the
/// last harness line is redacted), two crashes (after the fact, before the
/// rename; then after the rename, before the cursor's re-baseline) and the
/// cursor lost between them. The latest fact matches the file, and what a
/// sync matching it read past its bytes is gone; but that is the redacted
/// line, which the ledger holds: not diverged. Opposed: the same truncation
/// of a line that is not redacted is diverged (the foreign-edit test).
#[test]
fn a_render_that_is_a_byte_prefix_of_the_old_file_is_not_diverged_after_two_crashes_and_a_lost_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, b) = plain_chat(dir.path());
    // A first turn, re-projected, so the binding's bound fact is where it stays.
    append(&b.loc, &harness_lines(&session(&b), &leaf(&b.loc), "2222"));
    sync::reproject(&mut s, &b.binding).unwrap();
    // A second turn: its lines, the last of which is then redacted.
    let lines = harness_lines(&session(&b), &leaf(&b.loc), "3333");
    append(&b.loc, &lines);
    assert!(!sync::sync(&mut s, &b.binding).unwrap().appended.is_empty());
    let whole = std::fs::read(&b.loc).unwrap();
    let last = lines.lines().last().unwrap();
    let last_uuid = serde_json::from_str::<Value>(last).unwrap()["uuid"].as_str().unwrap().to_string();
    let last_events: Vec<String> =
        ledger(&s, &b.agent).events.into_iter().filter(|e| e.native.as_ref().and_then(|n| n.id.as_deref()) == Some(last_uuid.as_str())).map(|e| e.id).collect();
    assert!(!last_events.is_empty());
    s.redact(&b.agent, &last_events, "test: the last line").unwrap();
    sync::reproject_with(&mut s, &b.binding, &mut crash_at("fact")).unwrap_err();
    let f = facts(&s, &b.agent, &b.binding).pop().unwrap();
    assert!(f.matches(&whole) && f.bytes < whole.len() as u64, "sentinel: the new render is a byte-prefix of the old file");
    drop(s);
    std::fs::remove_file(dir.path().join("store/index.db")).unwrap();
    let mut s = Store::open(dir.path().join("store")).unwrap();
    sync::reproject_with(&mut s, &b.binding, &mut crash_at("renamed")).unwrap_err();
    assert_eq!(std::fs::read(&b.loc).unwrap().len() as u64, f.bytes, "sentinel: the render is in place");
    let cursor = sync::Cursor::parse(&s.cursor(&b.binding).unwrap().unwrap()).unwrap();
    assert!(
        cursor.past.values().any(|p| p.values().flatten().any(|id| last_events.contains(id))),
        "sentinel: the redacted line was read past the fact's bytes"
    );
    let r = sync::sync(&mut s, &b.binding).unwrap();
    assert!(r.diverged.is_none(), "protoledger's own render was diverged: {r:?}");
}

// ---- carry and Codex ----

/// Re-projection renders with carry, as `project` does: in api-error the
/// redacted error line is followed by offset-named lines (`last-prompt`,
/// `cost-state`), which land away from their offsets; every event of the
/// projected view comes back under its own id.
#[test]
fn reproject_renders_with_carry_every_event_comes_back_under_its_own_id_across_the_gap() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, b) = projected(dir.path(), API_ERROR);
    let l = ledger(&s, &b.agent);
    // The isApiErrorMessage line imports as an `error` event.
    let error = l.events.iter().find(|e| e.native.as_ref().and_then(|n| n.id.as_deref()) == Some("03412cb5-2957-432f-bad2-663132335563")).unwrap().clone();
    assert_eq!((error.kind, error.body["message"].as_str()), (Kind::Error, Some("Credit balance is too low")));
    let at = l.events.iter().position(|e| e.id == error.id).unwrap();
    let after: Vec<String> = l.events[at + 1..].iter().filter(|e| e.id.starts_with('@') && e.native.is_some()).map(|e| e.id.clone()).collect();
    assert!(!after.is_empty(), "sentinel: offset-named events follow the error line");
    redact(&mut s, &b.agent, &error.id);
    sync::reproject(&mut s, &b.binding).unwrap();
    let back = import_ids(&b.loc, &b.agent);
    let view: Vec<String> = ledger(&s, &b.agent).projected_view().iter().map(|e| e.id.clone()).collect();
    let missing: Vec<&String> = view.iter().filter(|id| !back.contains(id)).collect();
    assert!(missing.is_empty(), "events of the view not back under their ids: {missing:?}");
    assert!(after.iter().all(|id| back.contains(id)), "an offset-named line came back under another id");
    assert!(!back.contains(&error.id));
}

/// Codex: a rollout writes the user's prompt twice, as an `event_msg`
/// echo and as the `response_item` message the model saw. Redacting only
/// the message leaves the echo, which the importer then reads as the
/// prompt, an event the ledger never held: the render does not import back
/// as itself, and re-projection refuses, writing nothing (step 3). Redacting
/// both halves (SPEC §11.1: a caller SHOULD redact both), the rollout is
/// re-projected, every other event comes back under its own id (carry), and
/// the shared `session_index.jsonl` beside it is never rewritten: after a
/// rename of the thread with its old name redacted, the old lines stay where
/// they were (a redacted name stays in Codex's index) and one new line is
/// appended to the same file.
#[test]
fn a_codex_rollout_reprojects_and_its_session_index_is_never_rewritten() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path().join("store")).unwrap();
    let imp = s.import("codex", &fixture("codex/legacy-session-index-synthetic/native")).unwrap();
    let agent = imp.created.iter().find(|a| a.ends_with("a001")).unwrap().clone();
    let home = dir.path().join("codex");
    let b = sync::project(&mut s, &agent, "codex", &home).unwrap();
    let loc = PathBuf::from(&b.location);
    assert_eq!(facts(&s, &agent, &b.binding).len(), 1, "a codex rollout gets project's fact");
    let index = home.join("session_index.jsonl");
    let index_before = std::fs::read(&index).unwrap();
    let before = std::fs::read(&loc).unwrap();
    let l = ledger(&s, &agent);
    let prompt = l.events.iter().find(|e| e.kind == Kind::Text && e.str("role") == Some("user")).unwrap().clone();
    let at = l.events.iter().position(|e| e.id == prompt.id).unwrap();
    let echo = l.events[at - 1].clone();
    assert!(
        echo.native.as_ref().and_then(|n| n.record_text()).is_some_and(|r| r.contains("\"user_message\"")),
        "sentinel: the echo before the prompt: {echo:?}"
    );

    redact(&mut s, &agent, &prompt.id);
    let n = ledger(&s, &agent).events.len();
    let e = sync::reproject(&mut s, &b.binding).unwrap_err();
    assert!(format!("{e:#}").contains("does not import back as itself"), "{e:#}");
    assert_eq!((std::fs::read(&loc).unwrap(), ledger(&s, &agent).events.len()), (before, n), "a refused re-projection wrote or appended");

    redact(&mut s, &agent, &echo.id);
    // The thread is renamed, and its current name redacted: re-projection
    // renders an index that differs from the one on disk (the old name
    // left out, a new one in).
    let index_ino = inode(&index);
    let renamed = l.events.iter().rev().find(|e| e.kind == Kind::Title).unwrap().clone();
    assert_eq!(renamed.str("text"), Some("Demo thread (renamed)"), "sentinel: the thread's current name");
    redact(&mut s, &agent, &renamed.id);
    let title = Event::new("test:title:third", Kind::Title).ts(Some("2026-10-12T09:30:00.000Z")).with("text", "Demo thread (third name)");
    s.append(&agent, title).unwrap();
    let r = sync::reproject(&mut s, &b.binding).unwrap();
    // session_index.jsonl is only appended to: the same file (inode), every
    // old line where it was (the redacted name included), and one new line
    // naming the new name.
    let index_now = std::fs::read(&index).unwrap();
    let text_before = String::from_utf8(index_before.clone()).unwrap();
    assert!(text_before.contains("\"thread_name\":\"Demo thread (renamed)\""), "sentinel: the old line naming the redacted name");
    assert_eq!(inode(&index), index_ino, "session_index.jsonl was replaced, not appended to");
    assert!(index_now.starts_with(&index_before), "session_index.jsonl was rewritten: an old line is gone or moved");
    let added: Vec<Value> = String::from_utf8(index_now[index_before.len()..].to_vec()).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(added.len(), 1, "{added:?}");
    assert_eq!(added[0]["thread_name"].as_str(), Some("Demo thread (third name)"), "{added:?}");
    assert_eq!(r.written, vec![loc.clone()], "the rollout alone is replaced");
    let f = formats::by_name("codex").unwrap();
    let back: Vec<String> = f.import(&loc, &ImportOptions::standard()).unwrap().set.get(&agent).unwrap().events.iter().map(|e| e.id.clone()).collect();
    assert!(!back.contains(&prompt.id) && !back.contains(&echo.id));
    let view: Vec<String> = ledger(&s, &agent).projected_view().iter().map(|e| e.id.clone()).collect();
    // The view's titles live in session_index.jsonl, not in the rollout, and
    // the codex exporter drops `app` events (the binding's own `bound` fact),
    // a named loss (`app-dropped`).
    let app_or_title: HashSet<String> = ledger(&s, &agent).events.into_iter().filter(|e| matches!(e.kind, Kind::App | Kind::Title)).map(|e| e.id).collect();
    let missing: Vec<&String> = view.iter().filter(|id| !back.contains(id) && !app_or_title.contains(*id)).collect();
    assert!(missing.is_empty(), "codex events not back under their ids: {missing:?}");
    assert!(view.iter().filter(|id| !app_or_title.contains(*id)).count() >= 10, "sentinel: the comparison covers the rollout's events");
    assert!(sync::sync(&mut s, &b.binding).unwrap().diverged.is_none());
}

// ---- the CLI ----

/// `protoledger reproject <store> <binding>`: prints the binding's fact; a
/// refused binding fails with the reason.
#[test]
fn reproject_cli_prints_the_fact_and_refuses_an_unbound_binding() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, b) = plain_chat(dir.path());
    let answer = event_with_text(&s, &b.agent, ANSWER);
    redact(&mut s, &b.agent, &answer.id);
    drop(s);
    let bin = env!("CARGO_BIN_EXE_protoledger");
    let store = dir.path().join("store");
    let out = std::process::Command::new(bin).args(["reproject"]).arg(&store).arg(&b.binding).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let file = std::fs::read(&b.loc).unwrap();
    assert_eq!((v["bytes"].as_u64(), v["sha256"].as_str()), (Some(file.len() as u64), Some(sha256_hex(&file).as_str())), "{v}");
    let un = std::process::Command::new(bin).args(["unbind"]).arg(&store).arg(&b.binding).output().unwrap();
    assert!(un.status.success());
    let out = std::process::Command::new(bin).args(["reproject"]).arg(&store).arg(&b.binding).output().unwrap();
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("not active"), "{}", String::from_utf8_lossy(&out.stderr));
}
