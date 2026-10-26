//! A simulated Claude Code turn on a projected ledger.
//!
//! The shapes come from `fixtures/claude-code/file-edit` (Claude Code
//! 2.1.280, a real run: its `SOURCE.md`): a user prompt with its attachments,
//! then three assistant messages, each written over several lines that share
//! one `message.id` — thinking, then text, then `tool_use`; the second message
//! also writes a second `tool_use` line after the first call's result — and
//! every line of a message carrying the message's final `usage` and
//! `stop_reason`, as 2.1.2xx writes them. The simulator appends that
//! session's lines to the projected file one at a time, each after the delay
//! the fixture's own timestamps show between it and the line before (a line
//! with no timestamp: 5 ms), with each write a separate open, append and
//! close, as Claude Code's `appendFileSync` does. One line (the text line of
//! the second message) is written in two pieces: its first half is flushed
//! with no newline, held for [`HOLD`] (past one poll period and the debounce,
//! so the follower syncs while that line is half written), then completed.
//! The sync engine declines to read a file whose final line has no newline
//! (`torn` in `src/sync`), so those syncs are deferred: the test counts them
//! as sync calls and counts separately how often the cursor moved (a read).
//! A partial line read by a follow is held by the format's follow itself;
//! that path is reached only when the torn-line check is bypassed, and
//! `tests/follow.rs` asserts it directly.

use crate::common::{self, Mark, Paths};
use crate::modes::Mode;
use protoledger::formats::{self, ImportOptions};
use protoledger::store::Store;
use protoledger::sync;
use protoledger::{Event, Kind};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const SOURCE: &str = "claude-code/file-edit/native/bcbbbce5-971a-443f-bf65-8c3ab40cc24b.jsonl";
const SOURCE_SESSION: &str = "bcbbbce5-971a-443f-bf65-8c3ab40cc24b";
/// The line written in two pieces: the text line of the second message.
const SPLIT_LINE: &str = "8f376f3a-";
/// How long the half-written line stays half written: one poll period (1 s)
/// and the debounce (150 ms), and some. `PL_SIM_FORCE=short-hold` makes it 0,
/// so no sync falls while the line is half written.
const HOLD: Duration = Duration::from_millis(1300);

fn import_ids(loc: &Path, agent: &str) -> Vec<String> {
    let imp = formats::by_name("claude-code").unwrap().import(loc, &ImportOptions::standard()).unwrap();
    imp.set.get(agent).unwrap_or_else(|| panic!("{} does not import as {agent}", loc.display())).events.iter().map(|e| e.id.clone()).collect()
}

/// The lines of `bytes` from byte `from` on, by the native id the importer
/// gives each: its `uuid`, or `@<byte offset>` for a line without one.
fn lines_by_id(bytes: &[u8], from: usize) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut at = from;
    for line in bytes[from..].split_inclusive(|b| *b == b'\n') {
        let text = std::str::from_utf8(line).unwrap().trim_end_matches('\n').to_string();
        let v: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("line at {at}: {e}"));
        let id = v.get("uuid").and_then(|u| u.as_str()).map(String::from).unwrap_or_else(|| format!("@{at}"));
        out.push((id, text));
        at += line.len();
    }
    out
}

/// One line the simulator writes, and how long after the previous one.
struct Line {
    text: String,
    delay: Duration,
}

fn ts(v: &Value) -> Option<f64> {
    // "2026-10-16T23:35:27.292Z": seconds within the day are enough for deltas.
    let t = v.get("timestamp")?.as_str()?;
    let hms = t.split('T').nth(1)?.trim_end_matches('Z');
    let mut it = hms.split(':');
    let (h, m, s): (f64, f64, f64) = (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    Some(h * 3600.0 + m * 60.0 + s)
}

/// The session's lines, as Claude Code resuming `session` after `parent`
/// writes them: the fixture's lines with its session id replaced and its first
/// message's null parent pointed at `parent`, paced by the fixture's
/// timestamps. With `old_usage` (`PL_SIM_FORCE=old-usage`), every line of a
/// message but its last carries a partial output count and no stop reason:
/// usage on a line that does not settle the message (what Claude Code wrote
/// before 2.1.2xx, as `fixtures/claude-code/v2.0.22-inline-sidechain` shows).
fn session_lines(session: &str, parent: &str, old_usage: bool) -> Vec<Line> {
    let text = std::fs::read_to_string(common::fixture(SOURCE)).unwrap();
    let text = text.replace(SOURCE_SESSION, session).replacen("\"parentUuid\":null", &format!("\"parentUuid\":\"{parent}\""), 1);
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if old_usage {
        let mid = |l: &str| serde_json::from_str::<Value>(l).ok().and_then(|v| v["message"]["id"].as_str().map(str::to_string));
        let last: HashMap<String, usize> = lines.iter().enumerate().filter_map(|(i, l)| mid(l).map(|m| (m, i))).collect();
        for (i, l) in lines.iter_mut().enumerate() {
            if let Some(m) = mid(l)
                && last[&m] != i
            {
                let mut v: Value = serde_json::from_str(l).unwrap();
                v["message"]["usage"]["output_tokens"] = Value::from(1);
                v["message"]["stop_reason"] = Value::Null;
                *l = serde_json::to_string(&v).unwrap();
            }
        }
        eprintln!("sim: PL_SIM_FORCE=old-usage: partial counts and no stop reason on every line of a message but its last");
    }
    let mut prev: Option<f64> = None;
    lines
        .into_iter()
        .map(|l| {
            let t = ts(&serde_json::from_str(&l).unwrap());
            let delay = match (prev, t) {
                (Some(a), Some(b)) if b >= a => Duration::from_secs_f64((b - a).min(1.5)),
                _ => Duration::from_millis(5),
            };
            if t.is_some() {
                prev = t;
            }
            Line { text: l, delay }
        })
        .collect()
}

fn append(path: &Path, bytes: &[u8]) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(bytes).unwrap();
}

/// The usage each message's last line states (what Claude Code finally
/// counted), by message id, from the bytes the turn wrote.
fn final_usage(lines: &[(String, String)]) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for (_, t) in lines {
        let v: Value = serde_json::from_str(t).unwrap();
        if v["type"] == "assistant"
            && let Some(m) = v["message"]["id"].as_str()
        {
            out.insert(m.to_string(), v["message"]["usage"]["output_tokens"].clone());
        }
    }
    out
}

/// Project `protoledger/minimal` into a temporary Claude Code projects
/// directory, follow it with the default options, let the simulator write a
/// turn, and check the live-sync properties on the ledger.
#[test]
fn claude_code_simulated_turn_once_each_exact_within_2s_syncs_mid_line_no_divergence_no_reimport_for_a_1s_poll_after() {
    claude_code_turn(Mode::Plain);
}

/// The test above in a store in `m` (retrained, in that mode, once the
/// session is projected and before the turn).
fn claude_code_turn(m: Mode) {
    let mut paths = Paths::new();
    let projects = paths.dir("claude/projects/-sim-work");
    let store_dir = paths.root.join("store");
    m.prepare(&store_dir);
    let mut s = Store::open(&store_dir).unwrap();
    let minimal = common::fixture("protoledger/minimal/native");
    paths.fixture(&minimal);
    paths.fixture(&common::fixture(SOURCE));
    s.import("protoledger", &minimal).unwrap();
    let b = sync::project(&mut s, "hello-1", "claude-code", &projects).unwrap();
    m.retrain(&mut s);
    common::live_sealing(m, &mut s);
    let loc = PathBuf::from(&b.location);
    paths.wrote(&loc);
    let session = loc.file_stem().unwrap().to_string_lossy().to_string();
    let pre_bytes = std::fs::read(&loc).unwrap();
    let pre_import = import_ids(&loc, "hello-1");
    let pre_len = s.reader("hello-1").unwrap().ledger().unwrap().events.len();
    let parent = lines_by_id(&pre_bytes, 0).into_iter().filter(|(id, _)| !id.starts_with('@')).map(|(id, _)| id).next_back().unwrap();

    let lines = session_lines(&session, &parent, common::force().as_deref() == Some("old-usage"));
    assert!(lines.iter().any(|l| l.text.contains(&format!("\"uuid\":\"{SPLIT_LINE}"))), "the line to split is in the source");
    let mut follower = common::follower(&s);
    let sim_loc = loc.clone();
    let hold = if common::force().as_deref() == Some("short-hold") {
        eprintln!("sim: PL_SIM_FORCE=short-hold: the split line is completed at once");
        Duration::ZERO
    } else {
        HOLD
    };
    let sim = move |tx: std::sync::mpsc::Sender<Mark>| {
        for l in &lines {
            std::thread::sleep(l.delay);
            let bytes = format!("{}\n", l.text);
            if l.text.contains(&format!("\"uuid\":\"{SPLIT_LINE}")) {
                let half = bytes.len() / 2;
                append(&sim_loc, &bytes.as_bytes()[..half]);
                let _ = tx.send(Mark::PartialStart(Instant::now()));
                std::thread::sleep(hold);
                let _ = tx.send(Mark::PartialEnd(Instant::now()));
                append(&sim_loc, &bytes.as_bytes()[half..]);
            } else {
                append(&sim_loc, bytes.as_bytes());
            }
        }
        let _ = tx.send(Mark::End(Instant::now()));
    };
    let want_loc = loc.clone();
    let pre_n = pre_import.len();
    let want = move || import_ids(&want_loc, "hello-1")[pre_n..].to_vec();
    // PL_SIM_FORCE=late-divergence: drop the file's last line once every
    // event is in, so the next sync finds lines it read gone.
    let cut_loc = loc.clone();
    let diverge = move || {
        let bytes = std::fs::read(&cut_loc).unwrap();
        let keep = bytes[..bytes.len() - 1].iter().rposition(|b| *b == b'\n').unwrap() + 1;
        std::fs::OpenOptions::new().write(true).open(&cut_loc).unwrap().set_len(keep as u64).unwrap();
    };
    let w = common::follow_turn(&mut s, follower.as_mut(), &b.binding, sim, want, diverge);
    let all = s.reader("hello-1").unwrap().ledger().unwrap().events;
    // First, before anything reads the location again: no sync up to a poll
    // period past the last event found it diverged or re-imported it whole.
    common::no_divergence_no_reimport(&all, &w, &b.binding, follower.as_ref(), "claude-code");

    // What the turn wrote, read back independently of sync.
    let bytes = std::fs::read(&loc).unwrap();
    assert!(bytes.len() > pre_bytes.len() && bytes.starts_with(&pre_bytes), "the simulator appended to the projected file");
    let now = import_ids(&loc, "hello-1");
    assert_eq!(now[..pre_import.len()], pre_import[..], "the importer reads the projected part as before");
    let want: Vec<String> = now[pre_import.len()..].to_vec();
    let got = &all[pre_len..];
    eprintln!(
        "sim: claude-code: {} new events from {} lines; {} syncs changed the store",
        want.len(),
        bytes[pre_bytes.len()..].split(|b| *b == b'\n').count() - 1,
        w.changed.len()
    );

    // 1. Once each, nothing else.
    common::once_each(got, &want, "claude-code", |_| false);
    // 2. Within 2 s of the turn's end.
    common::within_bound(&want, &w, "claude-code");
    // 3. Syncs while the line was half written: no divergence, no whole
    // re-import, up to a poll period past the last event.
    let mid = w.mid_line.expect("the simulator split a line");
    eprintln!(
        "sim: claude-code: {} syncs were made while a line was half written ({hold:?}); the cursor moved {} times then (a sync that read the file moves it; one the torn-line check deferred does not)",
        mid.syncs, mid.cursor_moves
    );
    assert!(mid.syncs >= 1, "no sync was made while the line was half written: the mid-line case was not exercised");
    common::read_mid_turn(&w, "claude-code");

    // 4. Native records, byte for byte: every line the turn wrote is the
    // record of exactly one event's part 0.
    let lines = lines_by_id(&bytes, pre_bytes.len());
    let by_id: HashMap<&str, &str> = lines.iter().map(|(id, t)| (id.as_str(), t.as_str())).collect();
    let mut covered: HashMap<&str, usize> = HashMap::new();
    for e in got.iter().filter(|e: &&Event| e.native.as_ref().is_some_and(|n| n.part == 0)) {
        let n = e.native.as_ref().unwrap();
        let nid = n.id.as_deref().unwrap_or_else(|| panic!("{}: a native record with no id", e.id));
        let text = n.raw.as_deref().or(n.record_text()).unwrap_or_else(|| panic!("{}: part 0 carries no record", e.id));
        let line = by_id.get(nid).unwrap_or_else(|| panic!("{}: native id {nid} is no line the turn wrote", e.id));
        assert_eq!(text, *line, "{}: the native record is not the line's bytes", e.id);
        *covered.entry(nid).or_default() += 1;
    }
    for (id, _) in &lines {
        assert_eq!(covered.get(id.as_str()).copied().unwrap_or(0), 1, "line {id}: carried by exactly one event's part 0");
    }

    // 5. Usage: one event per message, with the count Claude Code finally wrote.
    let fin = final_usage(&lines);
    assert_eq!(fin.len(), 3, "the turn wrote three assistant messages");
    for (m, out) in &fin {
        let u: Vec<&Event> = got.iter().filter(|e| e.kind == Kind::Usage && e.id == format!("{m}#usage")).collect();
        assert_eq!(u.len(), 1, "{m}: one usage event");
        assert_eq!(u[0].body.get("output"), Some(out), "{m}: the ledger books the usage Claude Code finally wrote for the message");
    }
    eprintln!("sim: claude-code: {} native lines equal byte for byte; usage of {} messages as finally written", lines.len(), fin.len());

    paths.store(&s);
    paths.assert_isolated("claude-code");
    drop(s);
    m.check(&store_dir);
}

modes_tests!(claude_code_turn);
