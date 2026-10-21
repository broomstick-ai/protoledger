//! SPEC §9.3, an importer is prefix-stable: importing any
//! prefix of a native source that ends on a record boundary yields exactly
//! the first events of importing the whole source (same ids, same bodies,
//! same native records); nothing an importer emits depends on records not yet
//! written.
//!
//! For every transcript of every claude-code and codex fixture (`.jsonl`, and
//! the decompressed lines of a `.jsonl.zst`; the other files of the case stay
//! whole beside it) and every line boundary `k` of it:
//!
//! - **prefix**: every ledger `import(prefix_k)` makes is in `import(whole)`
//!   and its events are, byte for byte (`id`, `seq`, `body`, `native` record
//!   as written), the first events of that ledger there.
//! - **prefix + follow**: where the format follows the file, the follow of
//!   `prefix_k` fed the rest either yields what, added to `import(prefix_k)`,
//!   is `import(whole)` byte for byte (headers too), or declines — and then
//!   sync re-imports the whole, which by the first property only adds events.
//!   A decline is allowed only where something other than events moved (a
//!   header or the variant): a decline over events alone would hide the
//!   instability this test is for.
//!
//! The number of (file, k) pairs checked is printed and must not be zero.
//! A codex `ts-json` file is one JSON document, not records on lines, and is
//! not split.

use protoledger::formats::{self, Format, ImportOptions, Imported};
use protoledger::ledger::LedgerSet;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// Every line-record file of a format's fixtures: (the case's `native` dir, the file relative to it).
fn transcripts(format: &str) -> Vec<(PathBuf, PathBuf)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root().join(format)) else { return out };
    let mut cases: Vec<PathBuf> = rd.map(|e| e.unwrap().path()).collect();
    cases.sort();
    for case in cases {
        let native = case.join("native");
        if !native.is_dir() {
            continue; // a generator or tool, not a case
        }
        for e in walkdir::WalkDir::new(&native).sort_by_file_name() {
            let e = e.unwrap();
            let name = e.file_name().to_string_lossy();
            // A `.jsonl.zst` is readable only with the zstd feature.
            let lines = name.ends_with(".jsonl") || (cfg!(feature = "zstd") && name.ends_with(".jsonl.zst"));
            if e.file_type().is_file() && lines && name != "session_index.jsonl" {
                out.push((native.clone(), e.path().strip_prefix(&native).unwrap().to_path_buf()));
            }
        }
    }
    out
}

fn line_of<T: serde::Serialize>(x: &T) -> String {
    serde_json::to_string(x).unwrap()
}

/// A ledger set as bytes: every ledger's JSONL, in agent order.
fn bytes_of(set: &LedgerSet) -> Vec<(String, String)> {
    set.iter().map(|l| (l.id().to_string(), l.to_jsonl())).collect()
}

/// The first event of `prefix` that is not the same event of `whole`, as
/// (agent, index, prefix's line, whole's line or why there is none).
fn first_difference(prefix: &Imported, whole: &Imported) -> Option<(String, usize, String, String)> {
    for p in prefix.set.iter() {
        let Some(w) = whole.set.get(p.id()) else {
            let first = p.events.first().map(line_of).unwrap_or_default();
            return Some((p.id().to_string(), 0, first, "(no such agent in the whole import)".into()));
        };
        for (i, a) in p.events.iter().enumerate() {
            let (a, b) = (line_of(a), w.events.get(i).map(line_of));
            if b.as_deref() != Some(a.as_str()) {
                return Some((p.id().to_string(), i, a, b.unwrap_or_else(|| "(the whole import has fewer events)".into())));
            }
        }
    }
    None
}

/// Do the two imports agree on everything but events: variant, agents and headers?
fn same_headers(prefix: &Imported, whole: &Imported) -> bool {
    prefix.variant == whole.variant && prefix.set.iter().all(|p| whole.set.get(p.id()).is_some_and(|w| line_of(&p.header) == line_of(&w.header)))
}

/// `prefix` with `delta` added, as bytes: each agent's header from `delta`
/// when it has one there, and its events from `prefix` then `delta`.
fn sum(prefix: &LedgerSet, delta: &LedgerSet) -> Vec<(String, String)> {
    let mut ids: Vec<&str> = prefix.iter().map(|l| l.id()).chain(delta.iter().map(|l| l.id())).collect();
    ids.sort();
    ids.dedup();
    ids.into_iter()
        .map(|id| {
            let header = delta.get(id).or(prefix.get(id)).unwrap().header.clone();
            let mut text = line_of(&header) + "\n";
            for e in prefix.get(id).into_iter().chain(delta.get(id)).flat_map(|l| &l.events) {
                text += &line_of(e);
                text.push('\n');
            }
            (id.to_string(), text)
        })
        .collect()
}

#[derive(Default, Debug, Clone)]
struct Counts {
    files: usize,
    /// (file, k) pairs whose prefix import was compared with the whole import.
    pairs: usize,
    /// ... and whose prefix import was not a prefix of the whole one.
    unstable: usize,
    /// The first few of those: "file at line k: agent #i: prefix ≠ whole".
    examples: Vec<String>,
    /// (file, k) pairs where a follow of the prefix was fed the rest and
    /// yielded exactly the whole import.
    followed: usize,
    /// ... declined, where a header or the variant moved.
    declined: usize,
    /// ... declined, although only events could have moved.
    declined_over_events: usize,
    /// ... where the format does not follow the file.
    unfollowed: usize,
    /// Line boundaries not checked: the importer refused that prefix (no
    /// line it reads yet), so there is nothing to compare.
    skipped: usize,
    /// Files not checked at all: no complete line the importer reads.
    empty: usize,
    /// Follows whose own import was not the prefix import, follows that
    /// failed, and follows whose yield plus the prefix was not the whole
    /// import: each a failure, counted (not panicked in the file's thread) so
    /// the summary line is always printed first.
    follow_wrong: usize,
    /// Files whose whole import failed, and file threads that panicked.
    broken: usize,
}

impl Counts {
    fn add(&mut self, c: Counts) {
        self.files += c.files;
        self.pairs += c.pairs;
        self.unstable += c.unstable;
        self.followed += c.followed;
        self.declined += c.declined;
        self.declined_over_events += c.declined_over_events;
        self.unfollowed += c.unfollowed;
        self.skipped += c.skipped;
        self.empty += c.empty;
        self.follow_wrong += c.follow_wrong;
        self.broken += c.broken;
        self.examples.extend(c.examples);
    }
}

/// The lines of a transcript (decompressed if it is `.zst`), and how to write a prefix of them back.
fn read_lines(path: &Path) -> Vec<u8> {
    let raw = std::fs::read(path).unwrap();
    if path.to_string_lossy().ends_with(".zst") {
        return unzst(&raw);
    }
    raw
}

fn write_lines(path: &Path, text: &[u8]) {
    if path.to_string_lossy().ends_with(".zst") {
        std::fs::write(path, zst(text)).unwrap();
    } else {
        std::fs::write(path, text).unwrap();
    }
}

#[cfg(feature = "zstd")]
fn unzst(raw: &[u8]) -> Vec<u8> {
    zstd::decode_all(raw).unwrap()
}

#[cfg(feature = "zstd")]
fn zst(text: &[u8]) -> Vec<u8> {
    zstd::encode_all(text, 3).unwrap()
}

#[cfg(not(feature = "zstd"))]
fn unzst(_: &[u8]) -> Vec<u8> {
    unreachable!("a .zst transcript is listed only with the zstd feature")
}

#[cfg(not(feature = "zstd"))]
fn zst(_: &[u8]) -> Vec<u8> {
    unreachable!("a .zst transcript is listed only with the zstd feature")
}

fn check_file(f: &dyn Format, native: &Path, rel: &Path) -> Counts {
    let opts = ImportOptions::standard();
    let tmp = tempfile::tempdir().unwrap();
    for e in walkdir::WalkDir::new(native) {
        let e = e.unwrap();
        let to = tmp.path().join(e.path().strip_prefix(native).unwrap());
        if e.file_type().is_dir() {
            std::fs::create_dir_all(&to).unwrap();
        } else {
            std::fs::copy(e.path(), &to).unwrap();
        }
    }
    let path = tmp.path().join(rel);
    let bytes = read_lines(&path);
    // The whole source is its complete lines: a last line with no newline is not a record yet.
    let complete = &bytes[..bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1)];
    let mut bounds = vec![0];
    bounds.extend(complete.iter().enumerate().filter(|(_, b)| **b == b'\n').map(|(i, _)| i + 1));
    let name = format!("{}/{}", native.parent().unwrap().file_name().unwrap().to_string_lossy(), rel.display());
    write_lines(&path, complete);
    let whole = match f.import(&path, &opts) {
        Ok(w) => w,
        Err(_) if complete.is_empty() => return Counts { empty: 1, skipped: bounds.len(), ..Default::default() },
        Err(e) => return Counts { broken: 1, examples: vec![format!("{name}: whole import: {e:#}")], ..Default::default() },
    };
    let whole_bytes = bytes_of(&whole.set);
    let mut c = Counts { files: 1, ..Default::default() };
    for (k, &b) in bounds.iter().enumerate() {
        write_lines(&path, &complete[..b]);
        // A prefix the importer refuses (no line it reads yet) makes no events.
        let Ok(prefix) = f.import(&path, &opts) else {
            c.skipped += 1;
            continue;
        };
        c.pairs += 1;
        if let Some((agent, i, a, w)) = first_difference(&prefix, &whole) {
            c.unstable += 1;
            if c.examples.len() < 3 {
                c.examples.push(format!("{name} at line {k}: {agent} event {i}:\n  prefix: {a}\n  whole:  {w}"));
            }
        }
        let Some(followed) = f.follow(&path, &opts) else {
            c.unfollowed += 1;
            continue;
        };
        let wrong = |c: &mut Counts, what: String| {
            c.follow_wrong += 1;
            if c.examples.len() < 3 {
                c.examples.push(format!("{name}: {what}"));
            }
        };
        let (start, mut st) = match followed {
            Ok(x) => x,
            Err(e) => {
                wrong(&mut c, format!("follow failed at line {k}: {e:#}"));
                continue;
            }
        };
        if bytes_of(&start.set) != bytes_of(&prefix.set) {
            wrong(&mut c, format!("follow's own import at line {k} is not the prefix import"));
            continue;
        }
        match st.feed(&complete[b..]) {
            Ok(Some(d)) if sum(&prefix.set, &d) == whole_bytes => c.followed += 1,
            Ok(Some(_)) => wrong(&mut c, format!("import of {k} lines + follow of the rest is not the whole import")),
            Ok(None) if same_headers(&prefix, &whole) => c.declined_over_events += 1,
            Ok(None) => c.declined += 1,
            Err(e) => wrong(&mut c, format!("follow's feed failed after line {k}: {e:#}")),
        }
    }
    c
}

fn check_files(format: &str, files: &[(PathBuf, PathBuf)]) -> Counts {
    let f = formats::by_name(format).unwrap();
    let per: Vec<Counts> = std::thread::scope(|s| {
        let hs: Vec<_> = files.iter().map(|(n, r)| (r, s.spawn(move || check_file(f, n, r)))).collect();
        hs.into_iter()
            .map(|(r, h)| {
                h.join().unwrap_or_else(|p| {
                    let msg = p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
                    Counts { broken: 1, examples: vec![format!("{}: the file's thread panicked: {msg}", r.display())], ..Default::default() }
                })
            })
            .collect()
    });
    let mut t = Counts::default();
    for ((n, r), c) in files.iter().zip(per) {
        if c.unstable > 0 {
            eprintln!(
                "{format}: {}/{}: {} of {} prefixes unstable",
                n.parent().unwrap().file_name().unwrap().to_string_lossy(),
                r.display(),
                c.unstable,
                c.pairs
            );
        }
        t.add(c);
    }
    t
}

fn assert_stable(what: &str, c: &Counts) {
    assert_stable_but(what, c, 0);
}

/// [`assert_stable`], with exactly `unstable` (file, k) pairs allowed to differ.
fn assert_stable_but(what: &str, c: &Counts, unstable: usize) {
    eprintln!(
        "{what}: {} files, {} (file, k) pairs checked, {} unstable; follow: {} yielded the whole, {} declined (a header moved), {} declined over events alone, {} wrong, {} not followed; not checked: {} boundaries the importer refused, {} files with no line it reads, {} broken",
        c.files, c.pairs, c.unstable, c.followed, c.declined, c.declined_over_events, c.follow_wrong, c.unfollowed, c.skipped, c.empty, c.broken
    );
    assert_eq!((c.broken, c.follow_wrong), (0, 0), "{what}: {}", c.examples.join("\n"));
    assert!(c.files > 0 && c.pairs > c.files, "{what}: nothing was checked: {c:?}");
    assert!(
        c.unstable == unstable,
        "{what}: {} prefix imports are not a prefix of the whole import (pinned: {unstable}); first:\n{}",
        c.unstable,
        c.examples.join("\n")
    );
    assert_eq!(c.declined_over_events, 0, "{what}: the follow declined where only events could move");
}

#[test]
fn claude_code_imports_are_prefix_stable_at_every_line_of_every_fixture() {
    let c = check_files("claude-code", &transcripts("claude-code"));
    assert_stable("claude-code", &c);
    assert!(c.followed > 0, "{c:?}");
    // Every boundary of every claude-code transcript is checked: the importer
    // reads any prefix, the empty one included (measured: 1061 of 1061).
    assert_eq!((c.skipped, c.empty), (0, 0), "claude-code prefixes not checked: {c:?}");
}

#[test]
fn codex_imports_are_prefix_stable_at_every_line_of_every_fixture() {
    let files = transcripts("codex");
    // The .zst fixture is checked exactly when this build can read it.
    assert_eq!(files.iter().any(|(_, r)| r.to_string_lossy().ends_with(".zst")), cfg!(feature = "zstd"), "the .zst fixture is checked iff zstd is on");
    let c = check_files("codex", &files);
    assert_stable("codex", &c);
    assert!(c.followed > 0, "{c:?}");
}

// ---- every other line-oriented format ----

/// Every format whose fixtures hold line-record files (`.jsonl`), other than
/// claude-code and codex above. A format with none is whole-document JSON, a
/// database, or one file per record, and its doc says so.
const LINE_FORMATS: [&str; 10] = ["copilot-cli", "cursor", "droid", "gemini-cli", "hermes", "letta-code", "omp", "openclaw", "pi", "protoledger"];

/// Formats that are not prefix-stable, each for a reason its doc records,
/// with the number of (file, k) pairs that differ, pinned: a change in either
/// direction is a decision someone makes on purpose.
/// - pi, omp: a session tree is imported as its active path, read from the
///   latest leaf, so a prefix that ends on another branch has another path
///   (docs/formats/pi.md, omp.md "Prefix stability"). Every unstable file
///   holds a branch; every stable one holds none.
/// - cursor: a `stream-json` item (its deltas, then `completed`) is one
///   native record that grows as its lines arrive (docs/formats/cursor.md).
const NOT_PREFIX_STABLE: [(&str, usize); 3] = [("cursor", 264), ("omp", 9), ("pi", 9)];

fn line_format_is_prefix_stable(format: &str) {
    let files = transcripts(format);
    let c = check_files(format, &files);
    match NOT_PREFIX_STABLE.iter().find(|(f, _)| *f == format) {
        None => assert_stable(format, &c),
        Some((_, pinned)) => assert_stable_but(format, &c, *pinned),
    }
}

macro_rules! line_format_tests {
    ($($test:ident => $name:literal),* $(,)?) => {
        $(
            #[test]
            fn $test() {
                line_format_is_prefix_stable($name);
            }
        )*
    };
}

line_format_tests! {
    copilot_cli_imports_are_prefix_stable_at_every_line => "copilot-cli",
    cursor_imports_are_prefix_stable_at_every_line => "cursor",
    droid_imports_are_prefix_stable_at_every_line => "droid",
    gemini_cli_imports_are_prefix_stable_at_every_line => "gemini-cli",
    hermes_imports_are_prefix_stable_at_every_line => "hermes",
    letta_code_imports_are_prefix_stable_at_every_line => "letta-code",
    omp_imports_are_prefix_stable_at_every_line => "omp",
    openclaw_imports_are_prefix_stable_at_every_line => "openclaw",
    pi_imports_are_prefix_stable_at_every_line => "pi",
    protoledger_imports_are_prefix_stable_at_every_line => "protoledger",
}

/// The population is pinned: exactly the formats whose fixtures hold a
/// `.jsonl` file are tested above (with claude-code and codex), and every
/// other format holds none (a format joining either side is a
/// decision someone makes on purpose).
#[test]
fn the_line_formats_are_exactly_those_with_line_fixtures() {
    let mut with_lines: Vec<String> = Vec::new();
    for f in formats::all() {
        if !transcripts(f.name()).is_empty() {
            with_lines.push(f.name().to_string());
        }
    }
    with_lines.sort();
    let mut want: Vec<String> = LINE_FORMATS.iter().chain(&["claude-code", "codex"]).map(|s| s.to_string()).collect();
    want.sort();
    assert_eq!(with_lines, want);
}

// ---- Claude Code usage ----

fn jsonl(lines: &[serde_json::Value]) -> String {
    lines.iter().map(|l| l.to_string() + "\n").collect()
}

fn user_line(uuid: &str, parent: Option<&str>, text: &str) -> serde_json::Value {
    serde_json::json!({"type": "user", "uuid": uuid, "parentUuid": parent, "sessionId": "s1", "timestamp": "2026-01-01T00:00:01.000Z", "version": "2.1.280",
        "message": {"role": "user", "content": text}})
}

/// One line of a streamed assistant message `mid`, as Claude Code writes it:
/// one content block, the message's usage as it then stood, and its
/// `stop_reason` once the message has one.
fn assistant_line(uuid: &str, parent: &str, mid: &str, block: serde_json::Value, stop: Option<&str>, output: u64) -> serde_json::Value {
    serde_json::json!({"type": "assistant", "uuid": uuid, "parentUuid": parent, "sessionId": "s1", "timestamp": "2026-01-01T00:00:02.000Z", "version": "2.1.280",
        "message": {"id": mid, "type": "message", "role": "assistant", "model": "claude-x", "content": [block], "stop_reason": stop,
            "usage": {"input_tokens": 10, "cache_creation_input_tokens": 100, "cache_read_input_tokens": 0, "output_tokens": output}}})
}

fn import_case(files: &[(&str, String)], rel: &str) -> (tempfile::TempDir, Imported) {
    let dir = tempfile::tempdir().unwrap();
    for (r, text) in files {
        std::fs::write(dir.path().join(r), text).unwrap();
    }
    let imp = formats::by_name("claude-code").unwrap().import(&dir.path().join(rel), &ImportOptions::standard()).unwrap();
    (dir, imp)
}

/// A subagent's reply as Claude Code 2.1.280 streams it: a thinking line with
/// partial counts and no `stop_reason`, then the tool call that settles the
/// message with its final counts; then a one-line reply. Each message's usage
/// is written once, with the final counts, keyed by the message id, right
/// after the line that settles it, and the import is prefix-stable at every
/// line.
#[test]
fn claude_code_usage_is_written_once_on_the_line_that_settles_the_message() {
    let lines = [
        user_line("u0", None, "go"),
        assistant_line("a1", "u0", "m1", serde_json::json!({"type": "thinking", "thinking": "hmm", "signature": "sig"}), None, 3),
        assistant_line(
            "a2",
            "a1",
            "m1",
            serde_json::json!({"type": "tool_use", "id": "T1", "name": "Read", "input": {"file_path": "/x"}}),
            Some("tool_use"),
            226,
        ),
        serde_json::json!({"type": "user", "uuid": "u1", "parentUuid": "a2", "sessionId": "s1", "timestamp": "2026-01-01T00:00:03.000Z", "version": "2.1.280",
            "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "T1", "content": "x"}]}}),
        assistant_line("a3", "u1", "m2", serde_json::json!({"type": "text", "text": "done"}), Some("end_turn"), 12),
    ];
    let files = [("s1.jsonl", jsonl(&lines))];
    let (_dir, imp) = import_case(&files, "s1.jsonl");
    let l = imp.set.get("s1").unwrap();
    let usage: Vec<(String, Option<String>, serde_json::Value)> =
        l.events.iter().filter(|e| e.kind == protoledger::Kind::Usage).map(|e| (e.id.clone(), e.group.clone(), e.body["output"].clone())).collect();
    assert_eq!(
        usage,
        [("m1#usage".to_string(), Some("m1".to_string()), serde_json::json!(226)), ("m2#usage".to_string(), Some("m2".to_string()), serde_json::json!(12))]
    );
    // Right after the settling line's own events.
    let at = l.events.iter().position(|e| e.id == "m1#usage").unwrap();
    assert_eq!(l.events[at - 1].id, "a2", "{:?}", l.events.iter().map(|e| &e.id).collect::<Vec<_>>());
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("s1.jsonl"), &files[0].1).unwrap();
    let c = check_file(formats::by_name("claude-code").unwrap(), dir.path(), Path::new("s1.jsonl"));
    assert_stable("streamed message", &c);
}

/// Opposed: messages of one line each are prefix-stable whatever line the
/// usage is read from; this case is green under the last-line rule too.
#[test]
fn claude_code_one_line_messages_are_prefix_stable() {
    let lines = [
        user_line("u0", None, "hi"),
        assistant_line("a1", "u0", "m1", serde_json::json!({"type": "text", "text": "hello"}), Some("end_turn"), 5),
        user_line("u1", Some("a1"), "again"),
        assistant_line("a2", "u1", "m2", serde_json::json!({"type": "text", "text": "hello again"}), Some("end_turn"), 6),
    ];
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("s1.jsonl"), jsonl(&lines)).unwrap();
    let c = check_file(formats::by_name("claude-code").unwrap(), dir.path(), Path::new("s1.jsonl"));
    assert_stable("one-line messages", &c);
    assert_eq!(c.pairs, 5);
}

/// Every per-message usage the importer reads from a Claude Code fixture is
/// keyed by the message id (`<message.id>#usage`, `~n` if a message id
/// repeats), so it is the same event whichever of the message's lines a sync
/// read last. The two operands come from different places: the id is the
/// importer's; the message id is read here from the raw line the usage event
/// names as its native record (its `uuid`), not from anything the importer
/// made.
#[test]
fn claude_code_usage_ids_are_keyed_by_the_message_id() {
    let mut n = 0;
    for (native, rel) in transcripts("claude-code") {
        // uuid → message.id, from the transcript's raw lines (every transcript of the case).
        let mut mid_of: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for e in walkdir::WalkDir::new(&native) {
            let e = e.unwrap();
            if !e.file_name().to_string_lossy().ends_with(".jsonl") {
                continue;
            }
            for line in std::fs::read_to_string(e.path()).unwrap_or_default().lines() {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
                if let (Some(u), Some(m)) = (v["uuid"].as_str(), v["message"]["id"].as_str()) {
                    mid_of.insert(u.to_string(), m.to_string());
                }
            }
        }
        let imp = formats::by_name("claude-code").unwrap().import(&native.join(&rel), &ImportOptions::standard()).unwrap();
        for l in imp.set.iter() {
            for e in l.events.iter().filter(|e| e.kind == protoledger::Kind::Usage && e.body.get("cumulative").is_none()) {
                let line = e.native.as_ref().and_then(|n| n.id.clone()).unwrap_or_default();
                let mid = mid_of.get(&line).unwrap_or_else(|| panic!("{}: usage {} names no raw line with a message id ({line})", rel.display(), e.id));
                let want = format!("{mid}#usage");
                assert!(
                    e.id == want || e.id.strip_prefix(&want).is_some_and(|r| r.starts_with('~')),
                    "{}: usage {} on the line of message {mid}",
                    rel.display(),
                    e.id
                );
                n += 1;
            }
        }
    }
    eprintln!("claude-code usage events keyed by the message id of their raw line: {n}");
    assert!(n > 50, "{n}");
}

/// A persisted tool output that no line refers to yet (Claude Code writes
/// `tool-results/<name>` before the line that refers to it): the import names
/// the loss `tool-result-file-unreferenced` and makes no event for it; once
/// the referring line is written, the file is one `file_change` after that
/// line's events, and the import is prefix-stable across the two.
#[test]
fn claude_code_an_unreferenced_persisted_output_waits_for_its_line() {
    let path = "/home/u/.claude/projects/p/s1/tool-results/r1.txt";
    let mut referring = user_line("u1", Some("u0"), "see file");
    referring["toolUseResult"] = serde_json::json!({"persistedOutputPath": path});
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("s1/tool-results")).unwrap();
    std::fs::write(dir.path().join("s1/tool-results/r1.txt"), "a large output\n").unwrap();
    std::fs::write(dir.path().join("s1.jsonl"), jsonl(&[user_line("u0", None, "start")])).unwrap();
    let f = formats::by_name("claude-code").unwrap();
    let before = f.import(&dir.path().join("s1.jsonl"), &ImportOptions::standard()).unwrap();
    let codes: Vec<&str> = before.losses.0.iter().map(|l| l.code.as_str()).collect();
    assert!(codes.contains(&"tool-result-file-unreferenced"), "{codes:?}");
    let kinds = |imp: &Imported| -> Vec<protoledger::Kind> { imp.set.iter().flat_map(|l| l.events.iter().map(|e| e.kind)).collect() };
    assert!(!kinds(&before).contains(&protoledger::Kind::FileChange), "{:?}", kinds(&before));
    std::fs::write(dir.path().join("s1.jsonl"), jsonl(&[user_line("u0", None, "start"), referring])).unwrap();
    let after = f.import(&dir.path().join("s1.jsonl"), &ImportOptions::standard()).unwrap();
    // Opposed: referred to, the file is read, once, and the loss is gone.
    assert_eq!(kinds(&after).iter().filter(|k| **k == protoledger::Kind::FileChange).count(), 1, "{:?}", kinds(&after));
    assert!(!after.losses.0.iter().any(|l| l.code == "tool-result-file-unreferenced"));
    let c = check_file(f, dir.path(), Path::new("s1.jsonl"));
    assert_stable("unreferenced persisted output", &c);
}

// ---- end to end through sync ----

/// The regime this covers: a binding read while Claude Code is half-way
/// through writing one assistant message (one line per content block). A real
/// session (plain-chat, whose message `msg_011Cg6npEKMRvkcMsmVE3bcJ` spans two
/// lines) is appended to a projected session ONE LINE AT A TIME with a sync
/// after every line, both re-importing whole (`sync`) and following
/// (`sync_with`): the binding never diverges or is deferred, at least one sync
/// lands between two lines of one message, and the ledger ends holding every
/// event of the file once, byte for byte the whole import's.
#[cfg(feature = "sync")]
#[test]
fn a_sync_between_two_lines_of_one_assistant_message_keeps_the_binding_and_every_event() {
    use protoledger::store::Store;
    use protoledger::sync;
    use std::io::Write;
    let plain = root().join("claude-code/plain-chat/native/1ead304c-a7bc-443a-8455-a1778cc62693.jsonl");
    for follow in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(dir.path().join("store")).unwrap();
        s.import("protoledger", &root().join("protoledger/minimal/native")).unwrap();
        let b = sync::project(&mut s, "hello-1", "claude-code", &dir.path().join("cc")).unwrap();
        let loc = PathBuf::from(&b.location);
        let mut fl = sync::Follows::default();
        let session = loc.file_stem().unwrap().to_str().unwrap().to_string();
        let last_uuid = std::fs::read_to_string(&loc)
            .unwrap()
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["uuid"].as_str().map(str::to_string))
            .next_back()
            .unwrap();
        let text = std::fs::read_to_string(&plain).unwrap().replace("1ead304c-a7bc-443a-8455-a1778cc62693", &session).replacen(
            "\"parentUuid\":null",
            &format!("\"parentUuid\":\"{last_uuid}\""),
            1,
        );
        let mid = |l: &str| -> Option<String> {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            (v["type"] == "assistant").then(|| v["message"]["id"].as_str().map(str::to_string)).flatten()
        };
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        let mut mid_message = 0;
        for (k, line) in lines.iter().enumerate() {
            let mut f = std::fs::OpenOptions::new().append(true).open(&loc).unwrap();
            f.write_all(line.as_bytes()).unwrap();
            f.sync_all().unwrap();
            let r = if follow { sync::sync_with(&mut s, &b.binding, &mut fl).unwrap() } else { sync::sync(&mut s, &b.binding).unwrap() };
            assert!(r.diverged.is_none() && r.deferred.is_none(), "follow={follow}, after line {k}: {r:?}");
            if let Some(next) = lines.get(k + 1)
                && mid(line).is_some()
                && mid(line) == mid(next)
            {
                mid_message += 1;
            }
        }
        assert!(mid_message >= 1, "no sync landed inside a message");
        let ledger = protoledger::Ledger::read(&s.ledger_path("hello-1").unwrap()).unwrap();
        let whole = formats::by_name("claude-code").unwrap().import(&loc, &ImportOptions::standard()).unwrap();
        let want: Vec<String> = whole.set.get("hello-1").unwrap().events.iter().map(|e| line_of(&e.native)).collect();
        let want_ids: Vec<&str> = whole.set.get("hello-1").unwrap().events.iter().map(|e| e.id.as_str()).collect();
        // The ledger: the import's events (the binding facts are the ledger's own, between them).
        let got: Vec<&protoledger::Event> = ledger.events.iter().filter(|e| want_ids.contains(&e.id.as_str())).collect();
        let mut seen = std::collections::HashSet::new();
        assert!(ledger.events.iter().all(|e| seen.insert(e.id.clone())), "an event is in the ledger twice");
        assert_eq!(got.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), want_ids, "follow={follow}");
        assert_eq!(got.iter().map(|e| line_of(&e.native)).collect::<Vec<_>>(), want, "follow={follow}: native records");
        eprintln!("follow={follow}: {} lines appended one per sync, {mid_message} syncs inside a message, {} events held once", lines.len(), got.len());
    }
}
