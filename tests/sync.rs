//! Bindings and sync (SPEC §9.2, §9.3): project, attach, the generic sync
//! engine (dedupe, divergence, rewinds), the cursor across reindex, and the
//! follower's watcher, debounce and poll.
#![cfg(feature = "sync")]

use protoledger::formats::{self, ImportOptions};
use protoledger::store::Store;
use protoledger::store::bindings::{self, Fact};
use protoledger::sync::{self, FollowOptions, Follower, Trigger};
use protoledger::{Event, Kind};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn fixture(p: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(p)
}

/// Every event id of `agent`'s ledger, in order, read from the ledger file
/// itself (not the index).
fn ledger_ids(s: &Store, agent: &str) -> Vec<String> {
    let l = protoledger::Ledger::read(&s.ledger_path(agent).unwrap()).unwrap();
    l.events.iter().map(|e| e.id.clone()).collect()
}

fn ledger_events(s: &Store, agent: &str) -> Vec<Event> {
    protoledger::Ledger::read(&s.ledger_path(agent).unwrap()).unwrap().events
}

/// The event ids a format's importer reads from `path`, per agent: computed
/// by the importer directly, with no store and no sync involved.
fn import_ids(format: &str, path: &Path, agent: &str) -> Vec<String> {
    let f = formats::by_name(format).unwrap();
    let imp = f.import(path, &ImportOptions::standard()).unwrap();
    imp.set.get(agent).unwrap_or_else(|| panic!("{agent} not in import of {}", path.display())).events.iter().map(|e| e.id.clone()).collect()
}

fn append_bytes(path: &Path, bytes: &str) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(bytes.as_bytes()).unwrap();
    f.sync_all().unwrap();
}

fn assert_no_repeats(ids: &[String]) {
    let mut seen = HashSet::new();
    for id in ids {
        assert!(seen.insert(id), "{id} is in the ledger twice");
    }
}

/// The binding facts in an agent's ledger, in order.
fn facts(s: &Store, agent: &str) -> Vec<Fact> {
    ledger_events(s, agent).iter().filter_map(bindings::fact).collect()
}

const PLAIN_CHAT: &str = "claude-code/plain-chat/native/1ead304c-a7bc-443a-8455-a1778cc62693.jsonl";
const PLAIN_CHAT_SESSION: &str = "1ead304c-a7bc-443a-8455-a1778cc62693";

/// Lines of a real Claude Code session, as a harness resuming `session` would
/// append them after `parent`: the fixture's lines with their session id
/// replaced, and the first message's null parent pointed at `parent`.
fn claude_lines(session: &str, parent: &str) -> (String, Vec<String>) {
    let text = std::fs::read_to_string(fixture(PLAIN_CHAT)).unwrap();
    let text = text.replace(PLAIN_CHAT_SESSION, session).replacen("\"parentUuid\":null", &format!("\"parentUuid\":\"{parent}\""), 1);
    let uuids = text.lines().filter_map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["uuid"].as_str().map(str::to_string)).collect();
    (text, uuids)
}

fn last_uuid(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap();
    text.lines().filter_map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["uuid"].as_str().map(str::to_string)).next_back().unwrap()
}

/// Project a protoledger fixture into Claude Code, append a real session's
/// lines to the projected file by hand, and sync: exactly those events are
/// appended, once each, in order, with the appended lines as their exact
/// native records; a second sync appends nothing.
#[test]
fn project_to_claude_code_then_hand_appended_lines_sync_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path().join("store")).unwrap();
    s.import("protoledger", &fixture("protoledger/minimal/native")).unwrap();
    let before = ledger_ids(&s, "hello-1");
    assert_eq!(before, ["hello-1:u:1", "hello-1:t:1"]);

    let cc = dir.path().join("cc");
    let b = sync::project(&mut s, "hello-1", "claude-code", &cc).unwrap();
    assert_eq!((b.agent.as_str(), b.format.as_str(), b.mode.as_str(), b.active), ("hello-1", "claude-code", "projection", true));
    let loc = PathBuf::from(&b.location);
    assert!(loc.is_file() && loc.starts_with(std::path::absolute(&cc).unwrap()), "{}", b.location);
    // The binding is a fact in the ledger, and the index row is read from it.
    assert!(matches!(&facts(&s, "hello-1")[..], [Fact::Bound(x)] if x.binding == b.binding && x.location == b.location));
    assert_eq!(s.bindings().unwrap(), vec![b.clone()]);
    let bound = ledger_ids(&s, "hello-1");
    let bound_len = bound.len();
    // The two events and the binding's facts: `bound`, then the
    // `protoledger/projected` fact naming the file written (SPEC §11.2).
    assert_eq!(bound_len, 4);
    assert_eq!(bound[2], format!("protoledger/bound:{}", b.binding));
    let projected = bindings::projections(&ledger_events(&s, "hello-1"), &b.binding);
    assert!(!projected.legacy && projected.facts.len() == 1 && projected.facts[0].id == bound[3], "{projected:?}");
    assert!(projected.facts[0].data.matches(&std::fs::read(&loc).unwrap()));

    // Nothing changed at the location: nothing to append.
    let idle = sync::sync(&mut s, &b.binding).unwrap();
    assert!(!idle.changed() && idle.deferred.is_none(), "{idle:?}");

    let pre = import_ids("claude-code", &loc, "hello-1");
    let session = loc.file_stem().unwrap().to_str().unwrap().to_string();
    let (lines, uuids) = claude_lines(&session, &last_uuid(&loc));
    append_bytes(&loc, &lines);
    let post = import_ids("claude-code", &loc, "hello-1");
    assert_eq!(&post[..pre.len()], &pre[..], "the importer reads the projected part as before");
    let expected: Vec<String> = post[pre.len()..].to_vec();
    assert!(expected.len() >= uuids.len() && !uuids.is_empty(), "{} events for {} uuid lines", expected.len(), uuids.len());

    let r = sync::sync(&mut s, &b.binding).unwrap();
    let got: Vec<String> = r.appended.iter().map(|p| p.event.id.clone()).collect();
    assert_eq!(got, expected, "exactly the events the appended lines make, in order");
    let ids = ledger_ids(&s, "hello-1");
    assert_eq!(ids[..bound_len], bound[..], "what was there stays");
    assert_eq!(&ids[bound_len..], &expected[..]);
    assert_no_repeats(&ids);
    // Every line the harness wrote with a uuid is in the ledger, once.
    for u in &uuids {
        let n = ids.iter().filter(|id| *id == u || id.starts_with(&format!("{u}#"))).count();
        assert!(n >= 1, "line {u} did not come in");
        assert_eq!(ids.iter().filter(|id| *id == u).count(), 1, "{u}");
    }
    // Exact native records: each appended line is the record of its event.
    let records: HashSet<String> =
        ledger_events(&s, "hello-1").iter().filter_map(|e| e.native.as_ref().and_then(|n| n.record_text().map(str::to_string))).collect();
    for line in lines.lines() {
        assert!(records.contains(line), "not held byte for byte: {}", &line[..line.len().min(120)]);
    }

    // Again: nothing new.
    let again = sync::sync(&mut s, &b.binding).unwrap();
    assert!(again.appended.is_empty(), "{:?}", again.appended.iter().map(|p| &p.event.id).collect::<Vec<_>>());
    assert_eq!(ledger_ids(&s, "hello-1"), ids);
}

/// Attach a real Codex rollout whose tail has not been written yet, then
/// append the rest by hand: the ledger ends as exactly the import of the
/// whole file (plus the binding), each event once.
#[test]
fn attach_codex_then_appended_lines_sync_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path().join("store")).unwrap();
    let src = fixture("codex/legacy-0.142-compaction-real/native/rollout-2026-06-26T23-11-51-019f0634-3175-7702-b55c-e256f2966840.jsonl");
    let text = std::fs::read_to_string(&src).unwrap();
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    assert!(lines.len() > 20);
    let (head, rest) = lines.split_at(12);
    let codex = dir.path().join("codex");
    std::fs::create_dir_all(&codex).unwrap();
    let loc = codex.join(src.file_name().unwrap());
    std::fs::write(&loc, head.concat()).unwrap();

    let (b, r) = sync::attach(&mut s, "codex", &loc).unwrap();
    assert_eq!((b.format.as_str(), b.mode.as_str(), b.active), ("codex", "attachment", true));
    assert_eq!(r.created, std::slice::from_ref(&b.agent));
    let agent = b.agent.clone();
    let head_ids = import_ids("codex", &loc, &agent);
    assert_eq!(ledger_ids(&s, &agent)[..head_ids.len()], head_ids[..]);
    assert!(matches!(&facts(&s, &agent)[..], [Fact::Bound(x)] if x.binding == b.binding));

    append_bytes(&loc, &rest.concat());
    let whole = import_ids("codex", &src, &agent);
    assert_eq!(&whole[..head_ids.len()], &head_ids[..], "the head reads the same alone");
    let expected = &whole[head_ids.len()..];
    assert!(!expected.is_empty());

    let r = sync::sync(&mut s, &b.binding).unwrap();
    assert_eq!(r.appended.iter().map(|p| p.event.id.as_str()).collect::<Vec<_>>(), expected);
    assert!(r.diverged.is_none());
    let again = sync::sync(&mut s, &b.binding).unwrap();
    assert!(!again.changed(), "{again:?}");

    let mut ids = ledger_ids(&s, &agent);
    assert_no_repeats(&ids);
    ids.retain(|i| !i.starts_with(bindings::BOUND));
    assert_eq!(ids, whole, "the ledger is the whole file's import");
}

/// A Claude Code session imported into a store, projected back out, with its
/// projected file returned (it re-emits the native lines exactly).
struct Projected {
    agent: String,
    binding: String,
    location: PathBuf,
}

fn projected_plain_chat(root: &Path) -> (Store, Projected) {
    let mut s = Store::open(root.join("store")).unwrap();
    let imp = s.import("claude-code", &fixture(PLAIN_CHAT)).unwrap();
    let agent = imp.created[0].clone();
    let b = sync::project(&mut s, &agent, "claude-code", &root.join("cc")).unwrap();
    // Projecting into the directory the session came from would refuse to
    // overwrite; this is a fresh one.
    (s, Projected { agent, binding: b.binding, location: PathBuf::from(b.location) })
}

/// SPEC §9.3: an append at the tail and a rewind (a line continuing from an
/// earlier one) are not divergence; removing lines from the middle is: one
/// `protoledger/diverged` event, nothing else appended, and the binding is
/// no longer followed.
#[test]
fn divergence_is_a_middle_edit_not_a_tail_append_or_a_rewind() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, p) = projected_plain_chat(dir.path());
    let loc = &p.location;
    let original = std::fs::read_to_string(loc).unwrap();
    assert_eq!(original, std::fs::read_to_string(fixture(PLAIN_CHAT)).unwrap(), "the projection is the session, byte for byte");

    // A tail append: a rewind, continuing from the first user message.
    let first_user = original
        .lines()
        .find_map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            (v["type"] == "user").then(|| v["uuid"].as_str().unwrap().to_string())
        })
        .unwrap();
    let rewind = format!(
        "{{\"parentUuid\":\"{first_user}\",\"isSidechain\":false,\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"Try again.\"}},\"uuid\":\"0d6c6a5e-0000-4000-8000-00000000000a\",\"timestamp\":\"2026-10-16T23:40:00.000Z\",\"userType\":\"external\",\"cwd\":\"/home/user/work/plain-chat\",\"sessionId\":\"{PLAIN_CHAT_SESSION}\",\"version\":\"2.1.280\",\"gitBranch\":\"\"}}\n"
    );
    append_bytes(loc, &rewind);
    let r = sync::sync(&mut s, &p.binding).unwrap();
    assert!(r.diverged.is_none(), "a tail append is not divergence: {:?}", r.diverged);
    let rid = "0d6c6a5e-0000-4000-8000-00000000000a";
    assert!(!r.appended.is_empty());
    assert!(
        r.appended.iter().all(|x| x.event.id == rid || x.event.id.starts_with(&format!("{rid}#"))),
        "{:?}",
        r.appended.iter().map(|x| &x.event.id).collect::<Vec<_>>()
    );
    assert_eq!(r.appended[0].event.id, rid);
    // SPEC §2.3: the rewind continues from the last event the parent line made
    // (its text, `#1`, after a settings event), so the user's prompt stays on
    // the active path.
    assert_eq!(r.appended[0].event.after.as_deref(), Some(format!("{first_user}#1").as_str()), "the rewind names where it continues from");
    let ledger = protoledger::Ledger::read(&s.ledger_path(&p.agent).unwrap()).unwrap();
    let active = ledger.active_path();
    assert!(active.iter().any(|e| e.str("text").is_some_and(|t| t.starts_with("Say hello"))), "the user's prompt left the active path");
    assert!(active.iter().any(|e| e.id == rid));
    assert!(s.binding(&p.binding).unwrap().unwrap().active);

    // Remove three lines from the middle.
    let now = std::fs::read_to_string(loc).unwrap();
    let ls: Vec<&str> = now.split_inclusive('\n').collect();
    let cut: String = ls[..6].concat() + &ls[9..].concat();
    std::fs::write(loc, cut).unwrap();
    let before = ledger_ids(&s, &p.agent);
    let r = sync::sync(&mut s, &p.binding).unwrap();
    assert!(r.diverged.is_some(), "a middle edit is divergence");
    assert!(r.appended.is_empty() && r.created.is_empty(), "nothing else: {:?}", r.appended.iter().map(|x| &x.event.id).collect::<Vec<_>>());
    let after = ledger_ids(&s, &p.agent);
    assert_eq!(after.len(), before.len() + 1, "exactly one event");
    assert_eq!(after[..before.len()], before[..]);
    let last = ledger_events(&s, &p.agent).pop().unwrap();
    assert!(matches!(bindings::fact(&last), Some(Fact::Diverged { binding, .. }) if binding == p.binding));
    let row = s.binding(&p.binding).unwrap().unwrap();
    assert_eq!((row.active, row.ended.as_deref()), (false, Some("diverged")));

    // No longer followed: a further sync reads nothing and appends nothing.
    append_bytes(loc, &rewind.replace("0d6c6a5e-0000-4000-8000-00000000000a", "0d6c6a5e-0000-4000-8000-00000000000b"));
    let r = sync::sync(&mut s, &p.binding).unwrap();
    assert!(!r.changed() && r.deferred.is_some(), "{r:?}");
    assert_eq!(ledger_ids(&s, &p.agent), after);
}

/// The cursor is the one binding column not in the ledgers: it survives an
/// in-place reindex; deleting index.db loses it, the bindings come back from
/// the ledgers, and the next sync is a rescan that appends nothing.
#[test]
fn bindings_come_back_from_the_ledgers_and_the_cursor_survives_reindex() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, p) = projected_plain_chat(dir.path());
    let cursor = s.cursor(&p.binding).unwrap().expect("a cursor after bind");
    let rows = s.bindings().unwrap();
    s.reindex().unwrap();
    assert_eq!(s.bindings().unwrap(), rows);
    assert_eq!(s.cursor(&p.binding).unwrap().as_deref(), Some(cursor.as_str()), "kept across reindex");
    drop(s);

    for n in ["index.db", "index.db-wal", "index.db-shm"] {
        let _ = std::fs::remove_file(dir.path().join("store").join(n));
    }
    let mut s = Store::open(dir.path().join("store")).unwrap();
    assert_eq!(s.bindings().unwrap(), rows, "rebuilt from the ledgers");
    assert_eq!(s.cursor(&p.binding).unwrap(), None, "the cursor went with index.db");
    let before = ledger_ids(&s, &p.agent);
    let r = sync::sync(&mut s, &p.binding).unwrap();
    assert!(!r.changed(), "a rescan appends nothing: {r:?}");
    assert_eq!(ledger_ids(&s, &p.agent), before);
    assert!(s.cursor(&p.binding).unwrap().is_some(), "re-baselined");

    // Unbind is a ledger fact too.
    sync::unbind(&mut s, &p.binding).unwrap();
    let row = s.binding(&p.binding).unwrap().unwrap();
    assert_eq!((row.active, row.ended.as_deref()), (false, Some("unbound")));
    s.reindex().unwrap();
    assert_eq!(s.binding(&p.binding).unwrap().unwrap(), row);
    assert!(sync::unbind(&mut s, &p.binding).is_err());
}

/// Attaching and projecting are explicit and refuse to clobber: the same
/// location twice, or a projection onto existing files.
#[test]
fn attach_and_project_refuse_what_they_must() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, p) = projected_plain_chat(dir.path());
    assert!(sync::attach(&mut s, "claude-code", &p.location).is_err(), "already bound");
    let again = sync::project(&mut s, &p.agent, "claude-code", &dir.path().join("cc"));
    assert!(again.is_err(), "the projected file exists");
    // A torn final line (a write in progress) is not read: deferred, then read once complete.
    append_bytes(&p.location, "{\"type\":\"atis-latch\"");
    let r = sync::sync(&mut s, &p.binding).unwrap();
    assert!(r.deferred.is_some() && !r.changed(), "{r:?}");
    append_bytes(&p.location, &format!(",\"atis\":\"\",\"sessionId\":\"{PLAIN_CHAT_SESSION}\"}}\n"));
    let r = sync::sync(&mut s, &p.binding).unwrap();
    assert_eq!(r.appended.len(), 1, "{r:?}");
    assert!(r.diverged.is_none());
}

// ---- the follower ----

/// A codex rollout attached with its tail held back; returns the store, the
/// binding, the location, the held-back text and the ids it should make.
fn attached_codex(root: &Path) -> (Store, String, PathBuf, String, Vec<String>) {
    let mut s = Store::open(root.join("store")).unwrap();
    let src = fixture("codex/legacy-0.142-compaction-real/native/rollout-2026-06-26T23-11-51-019f0634-3175-7702-b55c-e256f2966840.jsonl");
    let text = std::fs::read_to_string(&src).unwrap();
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let (head, rest) = lines.split_at(12);
    std::fs::create_dir_all(root.join("codex")).unwrap();
    let loc = root.join("codex").join(src.file_name().unwrap());
    std::fs::write(&loc, head.concat()).unwrap();
    let (b, _) = sync::attach(&mut s, "codex", &loc).unwrap();
    let head_ids = import_ids("codex", &loc, &b.agent);
    let whole = import_ids("codex", &src, &b.agent);
    (s, b.binding, loc, rest.concat(), whole[head_ids.len()..].to_vec())
}

fn run_follower(s: &mut Store, f: &mut Follower, for_: Duration) -> Vec<(Trigger, sync::Synced)> {
    let until = Instant::now() + for_;
    let mut out = Vec::new();
    while Instant::now() < until {
        out.extend(f.step(s, until).unwrap());
    }
    out
}

/// The watcher delivers: with the poll at an hour it cannot be the poll.
#[test]
fn follower_watcher_delivers_an_append_with_the_poll_at_one_hour() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, binding, loc, rest, expected) = attached_codex(dir.path());
    let opts = FollowOptions { debounce: Duration::from_millis(100), poll: Duration::from_secs(3600), watcher: true };
    let mut f = Follower::new(&s, opts).unwrap();
    // Give the platform watcher a moment to arm before the write.
    std::thread::sleep(Duration::from_millis(300));
    append_bytes(&loc, &rest);
    let until = Instant::now() + Duration::from_secs(10);
    let mut got = Vec::new();
    while got.is_empty() && Instant::now() < until {
        got = f.step(&mut s, until).unwrap();
    }
    assert!(!got.is_empty(), "the watcher delivered nothing within 10 s");
    assert!(got.iter().all(|(t, _)| *t == Trigger::Watcher), "{:?}", got.iter().map(|(t, _)| t).collect::<Vec<_>>());
    let ids: Vec<String> = got.iter().flat_map(|(_, r)| r.appended.iter().map(|p| p.event.id.clone())).collect();
    assert_eq!(ids, expected);
    assert!(got.iter().all(|(_, r)| r.binding == binding));
}

/// With the watcher off, the poll delivers.
#[test]
fn follower_poll_delivers_an_append_with_the_watcher_off_poll_200ms() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, _binding, loc, rest, expected) = attached_codex(dir.path());
    let opts = FollowOptions { debounce: Duration::from_millis(100), poll: Duration::from_millis(200), watcher: false };
    let mut f = Follower::new(&s, opts).unwrap();
    append_bytes(&loc, &rest);
    let until = Instant::now() + Duration::from_secs(10);
    let mut got = Vec::new();
    while got.is_empty() && Instant::now() < until {
        got = f.step(&mut s, until).unwrap();
    }
    assert!(got.iter().all(|(t, _)| *t == Trigger::Poll), "{:?}", got.iter().map(|(t, _)| t).collect::<Vec<_>>());
    let ids: Vec<String> = got.iter().flat_map(|(_, r)| r.appended.iter().map(|p| p.event.id.clone())).collect();
    assert_eq!(ids, expected);
}

/// Nothing is appended for a location nobody wrote to, waited on for five
/// poll periods (200 ms) and ten debounce periods (100 ms); then a rewrite
/// of the same bytes (a change the stamp sees) is read and still appends
/// nothing.
#[test]
fn untouched_location_appends_nothing_past_debounce_100ms_and_poll_200ms() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, binding, loc, _rest, _expected) = attached_codex(dir.path());
    let agent = s.binding(&binding).unwrap().unwrap().agent;
    let before = ledger_ids(&s, &agent);
    let cursor = s.cursor(&binding).unwrap();
    let opts = FollowOptions { debounce: Duration::from_millis(100), poll: Duration::from_millis(200), watcher: true };
    let mut f = Follower::new(&s, opts).unwrap();
    let got = run_follower(&mut s, &mut f, Duration::from_millis(1000));
    assert!(got.is_empty(), "{} syncs changed the store", got.len());
    assert_eq!(ledger_ids(&s, &agent), before);
    assert_eq!(s.cursor(&binding).unwrap(), cursor, "the cursor is as it was: nothing new was read");

    // Same bytes, new mtime: read (the cursor's stamp moves), nothing appended.
    std::thread::sleep(Duration::from_millis(20));
    let bytes = std::fs::read(&loc).unwrap();
    std::fs::write(&loc, &bytes).unwrap();
    let got = run_follower(&mut s, &mut f, Duration::from_millis(1000));
    assert!(got.is_empty(), "{} syncs changed the store", got.len());
    assert_eq!(ledger_ids(&s, &agent), before);
    assert_ne!(s.cursor(&binding).unwrap(), cursor, "the rewrite was read");
}

/// A sync of a store event that is not a binding fact is not mistaken for one.
#[test]
fn only_app_events_of_the_binding_types_are_facts() {
    let e = Event::new("x", Kind::App).with("type", "other/bound").with("data", serde_json::json!({"binding": "b"}));
    assert_eq!(bindings::fact(&e), None);
    let e = Event::new("x", Kind::Text).with("type", bindings::UNBOUND).with("data", serde_json::json!({"binding": "b"}));
    assert_eq!(bindings::fact(&e), None);
    let e = Event::new("x", Kind::App).with("type", bindings::UNBOUND).with("data", serde_json::json!({"binding": "b"}));
    assert_eq!(bindings::fact(&e), Some(Fact::Unbound { binding: "b".into() }));
}

// ---- following a growing file ----

/// Change one byte of `path` at `at` to another ASCII letter, in place
/// (same file, same length).
fn flip_byte(path: &Path, at: usize) {
    use std::io::{Seek, SeekFrom};
    let bytes = std::fs::read(path).unwrap();
    assert!(bytes[at].is_ascii_lowercase(), "byte {at} is {:?}", bytes[at] as char);
    let b = if bytes[at] == b'z' { b'y' } else { bytes[at] + 1 };
    let mut f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.seek(SeekFrom::Start(at as u64)).unwrap();
    f.write_all(&[b]).unwrap();
    f.sync_all().unwrap();
}

/// The offset of the first lowercase letter in the string value of `key`
/// (its last occurrence) on the line starting at `line_start`: a byte whose
/// change keeps the line JSON.
fn letter_in(path: &Path, line_start: usize, key: &str) -> usize {
    let bytes = std::fs::read(path).unwrap();
    let end = bytes[line_start..].iter().position(|b| *b == b'\n').unwrap() + line_start;
    let line = std::str::from_utf8(&bytes[line_start..end]).unwrap();
    let at = line.rfind(&format!("\"{key}\":\"")).unwrap() + key.len() + 4;
    line_start + at + line[at..].find(|c: char| c.is_ascii_lowercase()).unwrap()
}

/// A bound file that grows is read on from where the last sync stopped
/// (`Synced::followed`), and what that appends is exactly the tail of the
/// whole file's import, each event once. Opposed: a file replaced at its path,
/// rewritten in place (a byte changed in its last line, then in its first), or
/// shortened, is read whole again (`followed` false).
#[test]
fn a_growing_file_is_read_from_where_the_last_sync_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, binding, loc, rest, expected) = attached_codex(dir.path());
    let agent = s.binding(&binding).unwrap().unwrap().agent;
    let mut fl = sync::Follows::default();
    let r = sync::sync_with(&mut s, &binding, &mut fl).unwrap();
    assert!(!r.followed && !r.changed(), "the first read is whole: {r:?}");
    assert_eq!(fl.bindings(), std::slice::from_ref(&binding), "and keeps the format's state");

    // Grow it a line at a time, all but the last two lines.
    let lines: Vec<&str> = rest.split_inclusive('\n').collect();
    let (grow, last) = lines.split_at(lines.len() - 2);
    let mut got: Vec<String> = Vec::new();
    let (mut followed, mut whole) = (0, 0);
    for line in grow {
        append_bytes(&loc, line);
        let r = sync::sync_with(&mut s, &binding, &mut fl).unwrap();
        assert!(r.diverged.is_none() && r.deferred.is_none(), "{r:?}");
        if r.followed {
            followed += 1
        } else {
            whole += 1
        }
        got.extend(r.appended.iter().map(|p| p.event.id.clone()));
    }
    eprintln!("{} lines appended one at a time: {followed} read from where the last sync stopped, {whole} whole", grow.len());
    assert!(followed > grow.len() / 2, "{followed} of {} syncs followed", grow.len());

    // Replaced: another file renamed over the path, with one more line.
    let now = std::fs::read(&loc).unwrap();
    let tmp = loc.with_extension("tmp");
    std::fs::write(&tmp, [now.as_slice(), last[0].as_bytes()].concat()).unwrap();
    std::fs::rename(&tmp, &loc).unwrap();
    let r = sync::sync_with(&mut s, &binding, &mut fl).unwrap();
    assert!(!r.followed && r.diverged.is_none(), "a replaced file is read whole: {r:?}");
    got.extend(r.appended.iter().map(|p| p.event.id.clone()));
    // ... and followed again after that.
    append_bytes(&loc, last[1]);
    let r = sync::sync_with(&mut s, &binding, &mut fl).unwrap();
    assert!(r.followed, "followed again after the whole read: {r:?}");
    got.extend(r.appended.iter().map(|p| p.event.id.clone()));
    assert_eq!(got, expected, "exactly the whole file's tail, in order");
    let mut ids = ledger_ids(&s, &agent);
    assert_no_repeats(&ids);
    ids.retain(|i| !i.starts_with(bindings::BOUND));
    assert_eq!(ids, import_ids("codex", &loc, &agent), "the ledger is the whole file's import");

    // Unchanged: read on, nothing to append.
    let r = sync::sync_with(&mut s, &binding, &mut fl).unwrap();
    assert!(r.followed && !r.changed(), "{r:?}");
    // Rewritten in place, its last line: read whole (nothing new by id).
    let text = std::fs::read_to_string(&loc).unwrap();
    let last_start = text[..text.len() - 1].rfind('\n').unwrap() + 1;
    flip_byte(&loc, letter_in(&loc, last_start, "type"));
    let r = sync::sync_with(&mut s, &binding, &mut fl).unwrap();
    assert!(!r.followed && !r.changed(), "a rewrite of the last line is read whole: {r:?}");
    let r = sync::sync_with(&mut s, &binding, &mut fl).unwrap();
    assert!(r.followed, "{r:?}");
    // Rewritten in place, its first line: read whole.
    flip_byte(&loc, letter_in(&loc, 0, "originator"));
    let r = sync::sync_with(&mut s, &binding, &mut fl).unwrap();
    assert!(!r.followed && !r.changed(), "a rewrite of the first line is read whole: {r:?}");
    // Shortened: read whole, and the lost line is divergence.
    let text = std::fs::read_to_string(&loc).unwrap();
    let cut = text[..text.len() - 1].rfind('\n').unwrap() + 1;
    std::fs::OpenOptions::new().write(true).open(&loc).unwrap().set_len(cut as u64).unwrap();
    let r = sync::sync_with(&mut s, &binding, &mut fl).unwrap();
    assert!(!r.followed && r.diverged.is_some(), "a shortened file is read whole: {r:?}");
    assert!(fl.bindings().is_empty(), "a diverged binding keeps no state");
}

/// The same for Claude Code: a projected session that a harness resumes,
/// line by line, is read on from where the last sync stopped.
#[test]
fn a_resumed_claude_code_session_is_read_from_where_the_last_sync_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Store::open(dir.path().join("store")).unwrap();
    s.import("protoledger", &fixture("protoledger/minimal/native")).unwrap();
    let b = sync::project(&mut s, "hello-1", "claude-code", &dir.path().join("cc")).unwrap();
    let loc = PathBuf::from(&b.location);
    let mut fl = sync::Follows::default();
    assert!(!sync::sync_with(&mut s, &b.binding, &mut fl).unwrap().followed);
    let pre = import_ids("claude-code", &loc, "hello-1");
    let session = loc.file_stem().unwrap().to_str().unwrap().to_string();
    let (lines, _) = claude_lines(&session, &last_uuid(&loc));
    let mut got: Vec<String> = Vec::new();
    let (mut followed, mut whole) = (0, 0);
    // A line at a time, the lines of one assistant message too: the import
    // is prefix-stable (SPEC §9.3), so a read between two of them
    // reads what a whole import does.
    let all: Vec<String> = lines.split_inclusive('\n').map(str::to_string).collect();
    for line in &all {
        append_bytes(&loc, line);
        let r = sync::sync_with(&mut s, &b.binding, &mut fl).unwrap();
        assert!(r.diverged.is_none() && r.deferred.is_none(), "{r:?}");
        if r.followed {
            followed += 1
        } else {
            whole += 1
        }
        got.extend(r.appended.iter().map(|p| p.event.id.clone()));
    }
    eprintln!("{} claude-code appends: {followed} followed, {whole} whole", all.len());
    assert!(followed > all.len() / 2, "{followed} of {} syncs followed", all.len());
    let post = import_ids("claude-code", &loc, "hello-1");
    assert_eq!(got, post[pre.len()..], "exactly the whole file's tail, in order");
    assert_no_repeats(&ledger_ids(&s, "hello-1"));
}

/// The poll reads a location only when its files' size or
/// modification time changed. Counted in reads, not inferred from the cursor:
/// over 1000 ms of 100 ms polls (ten periods) of an untouched location the
/// follower syncs nothing; opposed, a real append is then read.
#[test]
fn the_poll_reads_an_untouched_location_never_in_1000ms_of_100ms_polls() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, _binding, loc, rest, expected) = attached_codex(dir.path());
    let opts = FollowOptions { debounce: Duration::from_millis(50), poll: Duration::from_millis(100), watcher: false };
    let mut f = Follower::new(&s, opts).unwrap();
    let got = run_follower(&mut s, &mut f, Duration::from_millis(1000));
    assert!(got.is_empty());
    assert_eq!(f.syncs(), 0, "an untouched location was read {} times", f.syncs());
    append_bytes(&loc, &rest);
    let until = Instant::now() + Duration::from_secs(10);
    let mut got = Vec::new();
    while got.is_empty() && Instant::now() < until {
        got = f.step(&mut s, until).unwrap();
    }
    assert!(f.syncs() >= 1, "the append was never read");
    let ids: Vec<String> = got.iter().flat_map(|(_, r)| r.appended.iter().map(|p| p.event.id.clone())).collect();
    assert_eq!(ids, expected);
}

/// The follower watches active bindings only. Two locations are
/// bound; one is unbound behind the follower's back, and the next poll's
/// reconcile stops watching it, while the other is still watched. This
/// observes the follower's own filter (`reconcile`), with sync's refusal to
/// read an inactive binding left in place.
#[test]
fn the_follower_stops_watching_an_unbound_binding_and_keeps_an_active_one() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, codex, _loc, _rest, _expected) = attached_codex(dir.path());
    let cc = dir.path().join("cc-attached");
    std::fs::create_dir_all(&cc).unwrap();
    let cc_loc = cc.join("1ead304c-a7bc-443a-8455-a1778cc62693.jsonl");
    std::fs::copy(fixture(PLAIN_CHAT), &cc_loc).unwrap();
    let (claude, _) = sync::attach(&mut s, "claude-code", &cc_loc).unwrap();
    let opts = FollowOptions { debounce: Duration::from_millis(50), poll: Duration::from_millis(100), watcher: false };
    let mut f = Follower::new(&s, opts).unwrap();
    let mut both = vec![codex.clone(), claude.binding.clone()];
    both.sort();
    assert_eq!(f.followed(), both);
    sync::unbind(&mut s, &codex).unwrap();
    run_follower(&mut s, &mut f, Duration::from_millis(300));
    assert_eq!(f.followed(), std::slice::from_ref(&claude.binding), "the unbound binding is no longer watched; the active one is");
}

/// A followed Claude Code session whose sidecar (a subagent transcript in
/// `<session>/subagents/`) grows between two syncs: the session file itself
/// is unchanged, so only the sidecar check sends the sync to a whole
/// re-import (`followed` false), which brings the subagent's new events.
/// Opposed: a sync with no change at all is read on (`followed` true).
#[test]
fn a_sidecar_change_sends_a_followed_session_to_a_whole_re_import() {
    let dir = tempfile::tempdir().unwrap();
    let src = fixture("claude-code/subagent-sync/native");
    let sid = "3955358c-cf2e-4dd2-a240-838affed0178";
    let sub_rel = format!("{sid}/subagents/agent-a3f639a7c3c814bad.jsonl");
    let cc = dir.path().join("cc");
    for e in walkdir::WalkDir::new(&src).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()) {
        let to = cc.join(e.path().strip_prefix(&src).unwrap());
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(e.path(), &to).unwrap();
    }
    // The subagent's transcript with its tail not written yet.
    let sub = cc.join(&sub_rel);
    let text = std::fs::read_to_string(&sub).unwrap();
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    assert!(lines.len() > 4);
    let (head, rest) = lines.split_at(lines.len() / 2);
    std::fs::write(&sub, head.concat()).unwrap();

    let loc = cc.join(format!("{sid}.jsonl"));
    let mut s = Store::open(dir.path().join("store")).unwrap();
    let (b, _) = sync::attach(&mut s, "claude-code", &loc).unwrap();
    let child = "agent-a3f639a7c3c814bad";
    let head_ids = import_ids("claude-code", &loc, child);
    let mut fl = sync::Follows::default();
    let r = sync::sync_with(&mut s, &b.binding, &mut fl).unwrap();
    assert!(!r.followed && !r.changed(), "the first read is whole: {r:?}");
    assert_eq!(fl.bindings(), std::slice::from_ref(&b.binding), "the session is followed");

    // Opposed: nothing changed, read on from where the last sync stopped.
    let r = sync::sync_with(&mut s, &b.binding, &mut fl).unwrap();
    assert!(r.followed && !r.changed(), "an unchanged location is read on: {r:?}");

    // The sidecar grows; the session file does not.
    append_bytes(&sub, &rest.concat());
    let whole = import_ids("claude-code", &loc, child);
    let expected = &whole[head_ids.len()..];
    assert!(!expected.is_empty());
    let r = sync::sync_with(&mut s, &b.binding, &mut fl).unwrap();
    assert!(!r.followed, "a sidecar change is read whole: {r:?}");
    assert!(r.diverged.is_none(), "{r:?}");
    let got: Vec<&str> = r.appended.iter().filter(|p| p.agent == child).map(|p| p.event.id.as_str()).collect();
    assert_eq!(got, expected, "the subagent's new events arrive");
    assert_eq!(ledger_ids(&s, child), whole, "the subagent's ledger is its whole import");
}
