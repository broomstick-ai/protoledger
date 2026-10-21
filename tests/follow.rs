//! `Format::follow`: a growing claude-code or codex file, read
//! from a prefix on through the bytes appended to it, yields exactly the
//! ids, events and native records a whole re-import yields.
//!
//! For every `.jsonl` transcript of every claude-code and codex fixture, and
//! every line boundary `k` of it (the file's complete lines; a fixture whose
//! last line has no newline holds it, so "whole" is the file up to its last
//! newline):
//!
//! - **one shot**: the file holds its first `k` lines. `follow` must return
//!   exactly `import` of that file; the rest is then fed in one piece, and
//!   either the prefix import plus what the feed yields is, byte for byte,
//!   the whole import, or the feed declines (`None`) and the prefix import
//!   is indeed not a prefix of the whole one (a fallback that was needed).
//! - **inside a line**: the same, with the rest fed as two pieces split in
//!   the middle of its first line; the first piece yields nothing (the
//!   partial line is held), and the second yields what the one shot did.
//! - **chained**: one follow fed a line at a time from the start, compared
//!   after every line with the import of the file as it then is.
//!
//! The counts of what was checked are printed and must not be zero.

use protoledger::formats::{self, Format, ImportOptions, Imported};
use protoledger::ledger::LedgerSet;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// Every followed file of a format's fixtures: (the case's `native` dir, the file relative to it).
fn transcripts(format: &str) -> Vec<(PathBuf, PathBuf)> {
    let mut out = Vec::new();
    let mut cases: Vec<PathBuf> = std::fs::read_dir(root().join(format)).unwrap().map(|e| e.unwrap().path()).collect();
    cases.sort();
    for case in cases {
        let native = case.join("native");
        for e in walkdir::WalkDir::new(&native).sort_by_file_name() {
            let e = e.unwrap();
            let name = e.file_name().to_string_lossy();
            if e.file_type().is_file() && name.ends_with(".jsonl") && name != "session_index.jsonl" {
                out.push((native.clone(), e.path().strip_prefix(&native).unwrap().to_path_buf()));
            }
        }
    }
    out
}

/// A ledger set as bytes: every ledger's JSONL (header line, then events with
/// their native records as written), in agent order, and the variant.
fn bytes_of(set: &LedgerSet) -> Vec<(String, String)> {
    set.iter().map(|l| (l.id().to_string(), l.to_jsonl())).collect()
}

fn line_of<T: serde::Serialize>(x: &T) -> String {
    serde_json::to_string(x).unwrap()
}

/// Is `prefix` exactly the start of `whole`: same variant, every agent of
/// `prefix` in `whole` with the same header and its events the first of
/// `whole`'s?
fn is_extension(prefix: &Imported, whole: &Imported) -> bool {
    prefix.variant == whole.variant
        && prefix.set.iter().all(|p| {
            whole.set.get(p.id()).is_some_and(|w| {
                line_of(&p.header) == line_of(&w.header)
                    && p.events.len() <= w.events.len()
                    && p.events.iter().zip(&w.events).all(|(a, b)| line_of(a) == line_of(b))
            })
        })
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

#[derive(Default, Debug, Clone, Copy)]
struct Counts {
    files: usize,
    /// Files with no complete line (not followed, not counted in `files`).
    empty: usize,
    /// (file, split) pairs where the follow yielded and prefix + yield == whole.
    exact: usize,
    /// ... where it declined, and the prefix import is not a prefix of the whole.
    needed_fallback: usize,
    /// ... where it declined although the prefix import is a prefix of the whole.
    cautious_fallback: usize,
    /// ... where the format does not follow the prefix (no follow at all).
    unfollowed: usize,
    /// Splits inside a line checked.
    inside_line: usize,
    /// Chained feeds checked against the import of the file as it then was.
    chained: usize,
    /// Chained feeds that declined (each restarts the chain from that line):
    /// needed (the import before the line is no prefix of the import after
    /// it), or cautious (it is).
    chained_needed: usize,
    chained_cautious: usize,
}

fn check_file(f: &dyn Format, native: &Path, rel: &Path) -> Counts {
    let opts = ImportOptions::standard();
    let mut c = Counts { files: 1, ..Default::default() };
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
    let bytes = std::fs::read(&path).unwrap();
    let complete = &bytes[..bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1)];
    let mut bounds = vec![0];
    bounds.extend(complete.iter().enumerate().filter(|(_, b)| **b == b'\n').map(|(i, _)| i + 1));
    let name = format!("{}/{}", native.parent().unwrap().file_name().unwrap().to_string_lossy(), rel.display());
    std::fs::write(&path, complete).unwrap();
    let whole = match f.import(&path, &opts) {
        Ok(w) => w,
        // No complete line the format reads (a lone torn line): nothing to follow.
        Err(_) if complete.is_empty() => return Counts { files: 0, empty: 1, ..Default::default() },
        Err(e) => panic!("{name}: whole import: {e:#}"),
    };
    let whole_bytes = bytes_of(&whole.set);
    // The chain: a follow fed a line at a time, and the import it must equal.
    let mut chain: Option<(Box<dyn formats::Follow>, LedgerSet)> = None;
    let mut prev_import: Option<Imported> = None;
    for (k, &b) in bounds.iter().enumerate() {
        std::fs::write(&path, &complete[..b]).unwrap();
        let imported = f.import(&path, &opts);
        // The chain, one more line in.
        if let (Some((st, acc)), Ok(now)) = (chain.as_mut(), imported.as_ref()) {
            let line = &complete[bounds[k - 1]..b];
            match st.feed(line).unwrap() {
                Some(d) => {
                    let got = sum(acc, &d);
                    assert_eq!(got, bytes_of(&now.set), "{name}: chained feed of line {k}");
                    *acc = got_set(acc, &d);
                    c.chained += 1;
                }
                None => {
                    if prev_import.as_ref().is_some_and(|before| is_extension(before, now)) {
                        c.chained_cautious += 1;
                    } else {
                        c.chained_needed += 1;
                    }
                    chain = None;
                }
            }
        }
        let followed = f.follow(&path, &opts);
        let Some(followed) = followed else {
            c.unfollowed += 1;
            prev_import = imported.ok();
            continue;
        };
        let (start, mut st) = match followed {
            Ok(x) => x,
            Err(e) => {
                assert!(imported.is_err(), "{name}: follow failed at line {k} ({e:#}) where import did not");
                c.unfollowed += 1;
                prev_import = None;
                continue;
            }
        };
        let imported = imported.unwrap_or_else(|e| panic!("{name}: follow worked at line {k} where import failed: {e:#}"));
        assert_eq!(bytes_of(&start.set), bytes_of(&imported.set), "{name}: follow's own import at line {k}");
        assert_eq!(start.variant, imported.variant, "{name}: follow's variant at line {k}");
        // One shot.
        let rest = &complete[b..];
        let one = st.feed(rest).unwrap();
        match &one {
            Some(d) => {
                assert_eq!(sum(&imported.set, d), whole_bytes, "{name}: import of {k} lines + follow of the rest");
                assert!(is_extension(&imported, &whole), "{name}: a yield where the whole is no extension, at line {k}");
                c.exact += 1;
            }
            None if is_extension(&imported, &whole) => c.cautious_fallback += 1,
            None => c.needed_fallback += 1,
        }
        // Inside a line: the rest in two pieces, cut inside its first line.
        if k + 1 < bounds.len() && bounds[k + 1] - b >= 2 {
            let m = b + (bounds[k + 1] - b) / 2;
            let (_, mut st2) = f.follow(&path, &opts).unwrap().unwrap();
            let first = st2.feed(&complete[b..m]).unwrap();
            let first = first.unwrap_or_else(|| panic!("{name}: a partial line declined at line {k}"));
            assert!(first.iter().all(|l| l.events.is_empty()), "{name}: a partial line yielded events at line {k}");
            let second = st2.feed(&complete[m..]).unwrap();
            assert_eq!(second.as_ref().map(bytes_of), one.as_ref().map(bytes_of), "{name}: split inside line {}", k + 1);
            c.inside_line += 1;
        }
        if chain.is_none() {
            chain = Some((st_fresh(f, &path, &opts), imported.set.clone()));
        }
        prev_import = Some(imported);
    }
    c
}

/// `acc` with `d` added, as a set (headers from `d`).
fn got_set(acc: &LedgerSet, d: &LedgerSet) -> LedgerSet {
    let mut out = acc.clone();
    for l in d.iter() {
        let mut merged = match acc.get(l.id()) {
            Some(a) => a.clone(),
            None => protoledger::ledger::Ledger::new(l.agent().clone()),
        };
        *merged.agent_mut() = l.agent().clone();
        for e in &l.events {
            let seq = e.seq;
            merged.append(e.clone()).unwrap();
            merged.events.last_mut().unwrap().seq = seq;
        }
        out.insert(merged);
    }
    out
}

fn st_fresh(f: &dyn Format, path: &Path, opts: &ImportOptions) -> Box<dyn formats::Follow> {
    f.follow(path, opts).unwrap().unwrap().1
}

fn check_format(format: &str) -> Counts {
    let t = check_files(format, &transcripts(format));
    eprintln!("{format}: {t:?}");
    t
}

#[test]
fn claude_code_follow_yields_what_a_whole_import_does_at_every_split() {
    let c = check_format("claude-code");
    assert!(c.files > 0 && c.exact > 0 && c.inside_line > 0 && c.chained > 0, "{c:?}");
    // Declining is never the easy way out: on these fixtures it happens only
    // where the whole import really does change what came before.
    assert_eq!((c.cautious_fallback, c.chained_cautious), (0, 0), "{c:?}");
}

#[test]
fn codex_follow_yields_what_a_whole_import_does_at_every_split() {
    let c = check_format("codex");
    assert!(c.files > 0 && c.exact > 0 && c.inside_line > 0 && c.chained > 0, "{c:?}");
    assert_eq!((c.cautious_fallback, c.chained_cautious), (0, 0), "{c:?}");
}

/// Every file `format` writes when it exports (with carry, as `project`
/// does) the import of each fixture of `sources`: (the export's root, the
/// file relative to it). The export roots live in `keep`.
fn projections(format: &str, sources: &[&str], keep: &mut Vec<tempfile::TempDir>) -> Vec<(PathBuf, PathBuf)> {
    let to = formats::by_name(format).unwrap();
    let mut out = Vec::new();
    for src in sources {
        let from = formats::by_name(src).unwrap();
        let mut cases: Vec<PathBuf> = std::fs::read_dir(root().join(src)).unwrap().map(|e| e.unwrap().path()).collect();
        cases.sort();
        for case in cases {
            let Ok(imp) = from.import(&case.join("native"), &ImportOptions::standard()) else { continue };
            let Ok(ex) = to.export(&imp.set, &formats::ExportOptions::standard()) else { continue };
            let dir = tempfile::Builder::new().prefix(&format!("{src}-{}-", case.file_name().unwrap().to_string_lossy())).tempdir().unwrap();
            ex.output.write_scratch(dir.path()).unwrap();
            for rel in ex.output.files.keys().filter(|r| r.ends_with(".jsonl") && !r.ends_with("session_index.jsonl")) {
                out.push((dir.path().to_path_buf(), PathBuf::from(rel)));
            }
            keep.push(dir);
        }
    }
    out
}

fn check_files(format: &str, files: &[(PathBuf, PathBuf)]) -> Counts {
    let f = formats::by_name(format).unwrap();
    let per: Vec<Counts> = std::thread::scope(|s| {
        let hs: Vec<_> = files.iter().map(|(n, r)| s.spawn(move || check_file(f, n, r))).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut t = Counts::default();
    for c in per {
        t.files += c.files;
        t.empty += c.empty;
        t.exact += c.exact;
        t.needed_fallback += c.needed_fallback;
        t.cautious_fallback += c.cautious_fallback;
        t.unfollowed += c.unfollowed;
        t.inside_line += c.inside_line;
        t.chained += c.chained;
        t.chained_needed += c.chained_needed;
        t.chained_cautious += c.chained_cautious;
    }
    t
}

/// The same property on what `project` writes: every protoledger, claude-code
/// and codex fixture exported into Claude Code with carry (carried events and
/// headers, foreign events rendered as Claude Code lines).
#[test]
fn claude_code_follow_of_projected_ledgers_yields_what_a_whole_import_does() {
    let mut keep = Vec::new();
    let files = projections("claude-code", &["protoledger", "claude-code", "codex"], &mut keep);
    let c = check_files("claude-code", &files);
    eprintln!("claude-code projections: {c:?}");
    assert!(c.files > 0 && c.exact > 0 && c.inside_line > 0 && c.chained > 0, "{c:?}");
    assert_eq!((c.cautious_fallback, c.chained_cautious), (0, 0), "{c:?}");
}

/// ... and into Codex.
#[test]
fn codex_follow_of_projected_ledgers_yields_what_a_whole_import_does() {
    let mut keep = Vec::new();
    let files = projections("codex", &["protoledger", "claude-code", "codex"], &mut keep);
    let c = check_files("codex", &files);
    eprintln!("codex projections: {c:?}");
    assert!(c.files > 0 && c.exact > 0 && c.inside_line > 0 && c.chained > 0, "{c:?}");
    assert_eq!((c.cautious_fallback, c.chained_cautious), (0, 0), "{c:?}");
}

// ---- the lookaheads no fixture reaches ----
//
// Each case is a small transcript built so that one new line changes what an
// earlier line (or a header, or the import's variant) made, in a way only one
// of the follows' checks catches. `check_file` then needs that check: without
// it, the follow yields where the whole import is no extension of the prefix.

fn user(uuid: &str, parent: Option<&str>, text: &str) -> serde_json::Value {
    serde_json::json!({"type": "user", "uuid": uuid, "parentUuid": parent, "sessionId": "s1", "timestamp": "2026-01-01T00:00:01.000Z", "version": "2.1.280", "message": {"role": "user", "content": text}})
}

fn write_case(dir: &Path, files: &[(&str, String)]) {
    for (rel, text) in files {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
}

fn jsonl(lines: &[serde_json::Value]) -> String {
    lines.iter().map(|l| l.to_string() + "\n").collect()
}

/// Check one synthetic case and require that at least one split needed the
/// follow to decline.
fn check_case(format: &str, files: &[(&str, String)], followed: &str) -> Counts {
    let dir = tempfile::tempdir().unwrap();
    write_case(dir.path(), files);
    let c = check_file(formats::by_name(format).unwrap(), dir.path(), Path::new(followed));
    assert!(c.needed_fallback >= 1, "{followed}: no split needed a decline: {c:?}");
    c
}

/// A subagent's type named only by its spawning tool_result (a meta file that
/// names the call but not the type): the call's line gains `agent_type` when
/// the result arrives.
#[test]
fn claude_code_declines_when_a_tool_result_names_a_subagent_s_type() {
    let call = serde_json::json!({"type": "assistant", "uuid": "a1", "parentUuid": "u0", "sessionId": "s1", "timestamp": "2026-01-01T00:00:02.000Z",
        "message": {"id": "m1", "role": "assistant", "model": "claude-x", "content": [{"type": "tool_use", "id": "T1", "name": "Task", "input": {"prompt": "go"}}]}});
    let result = serde_json::json!({"type": "user", "uuid": "u2", "parentUuid": "a1", "sessionId": "s1", "timestamp": "2026-01-01T00:00:03.000Z",
        "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "T1", "content": "done"}]},
        "toolUseResult": {"agentId": "a1", "agentType": "general-purpose", "status": "completed", "content": [{"type": "text", "text": "done"}]}});
    let side = serde_json::json!({"type": "user", "uuid": "c1", "parentUuid": null, "sessionId": "s1", "agentId": "a1", "isSidechain": true, "timestamp": "2026-01-01T00:00:02.500Z", "message": {"role": "user", "content": "go"}});
    check_case(
        "claude-code",
        &[
            ("s1.jsonl", jsonl(&[user("u0", None, "start"), call, result])),
            ("s1/subagents/agent-a1.jsonl", jsonl(&[side])),
            ("s1/subagents/agent-a1.meta.json", r#"{"toolUseId":"T1"}"#.to_string()),
        ],
        "s1.jsonl",
    );
}

/// A line whose `parentUuid` is not in the file yet: when that line arrives
/// it joins an earlier line's conversation, and so changes that line's rewind.
/// Lines: u0; u2 (parent x, not yet written); m (parent u2); l (parent u2,
/// after m: a rewind past m, read alone); x (parent m), which puts m on l's
/// conversation, so l is no rewind after all.
#[test]
fn claude_code_declines_when_a_line_an_earlier_one_named_as_parent_arrives() {
    let lines = [user("u0", None, "a"), user("u2", Some("x"), "b"), user("m", Some("u2"), "c"), user("l", Some("u2"), "d"), user("x", Some("m"), "e")];
    check_case("claude-code", &[("s1.jsonl", jsonl(&lines))], "s1.jsonl");
}

/// A persisted tool output a subagent referenced first (in the prefix), then
/// the session: the whole import maps the session first, so the file moves.
#[test]
fn claude_code_declines_when_the_session_references_a_file_a_subagent_took() {
    let path = "/home/u/.claude/projects/p/s1/tool-results/r1.txt";
    let mut referencing = user("u1", Some("u0"), "see file");
    referencing["toolUseResult"] = serde_json::json!({"persistedOutputPath": path});
    let mut side = serde_json::json!({"type": "user", "uuid": "c1", "parentUuid": null, "sessionId": "s1", "agentId": "a1", "isSidechain": true, "timestamp": "2026-01-01T00:00:02.000Z", "message": {"role": "user", "content": "x"}});
    side["toolUseResult"] = serde_json::json!({"persistedOutputPath": path});
    check_case(
        "claude-code",
        &[
            ("s1.jsonl", jsonl(&[user("u0", None, "start"), referencing])),
            ("s1/subagents/agent-a1.jsonl", jsonl(&[side])),
            ("s1/tool-results/r1.txt", "a large output\n".to_string()),
        ],
        "s1.jsonl",
    );
}

/// The import's variant from the first version a transcript line states,
/// when no header shows it: the first line (not a transcript line) sets the
/// session's version; an inline sidechain line spelled with a space fixes the
/// session's own variant; the version then read moves the import's variant.
#[test]
fn claude_code_declines_when_the_import_s_variant_moves() {
    let first = serde_json::json!({"type": "custom-thing", "version": "9.9.9", "timestamp": "2026-01-01T00:00:00.000Z"});
    let side = "{\"type\":\"user\", \"isSidechain\": true, \"uuid\":\"s-1\",\"parentUuid\":null,\"sessionId\":\"s1\",\"timestamp\":\"2026-01-01T00:00:01.000Z\",\"message\":{\"role\":\"user\",\"content\":\"side\"}}\n";
    let mut main = user("u2", None, "main");
    main["version"] = serde_json::json!("1.0.50");
    let text = format!("{first}\n{side}{main}\n");
    check_case("claude-code", &[("s1.jsonl", text)], "s1.jsonl");
}

/// Two roots in one location (a subagent transcript that names another
/// session): forks are linked across roots by their events, so the location
/// is not followed; a new session line that opens the subagent's conversation
/// makes it a fork.
#[test]
fn claude_code_declines_a_location_with_two_roots() {
    let mut side = user("f1", None, "shared start");
    side["sessionId"] = serde_json::json!("other");
    side["agentId"] = serde_json::json!("z9");
    side["timestamp"] = serde_json::json!("2026-01-01T00:00:09.000Z");
    let dir = tempfile::tempdir().unwrap();
    write_case(dir.path(), &[("s1.jsonl", jsonl(&[user("u0", None, "a"), user("f1", None, "shared start")])), ("s1/subagents/agent-z9.jsonl", jsonl(&[side]))]);
    let c = check_file(formats::by_name("claude-code").unwrap(), dir.path(), Path::new("s1.jsonl"));
    assert_eq!((c.unfollowed, c.exact), (3, 0), "every split (0, 1 and 2 lines) is left to a whole import: {c:?}");
}

/// A rollout whose `session_meta` is not its first line: until it is read,
/// the thread is named after the file.
#[test]
fn codex_declines_when_the_header_moves() {
    let first = serde_json::json!({"timestamp": "2026-01-01T00:00:00.000Z", "type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}});
    let meta = serde_json::json!({"timestamp": "2026-01-01T00:00:01.000Z", "type": "session_meta", "payload": {"id": "0199a1b0-0000-7000-8000-000000000009", "timestamp": "2026-01-01T00:00:01.000Z", "cwd": "/w", "originator": "x", "cli_version": "0.163.0"}});
    let name = "rollout-2026-01-01T00-00-00-0199a1b0-0000-7000-8000-000000000009.jsonl";
    check_case("codex", &[(name, jsonl(&[first, meta]))], name);
}
