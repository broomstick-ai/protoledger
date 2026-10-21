//! JSONL plumbing shared by the line-oriented adapters in this crate section
//! (openclaw, droid): exact line capture, native-record re-emission and
//! whole-record carriers that can also hold the agent header.

use crate::carry;
use crate::formats::atif::jsonfmt::{hold, minify};
use crate::ledger::Ledger;
use crate::model::{Agent, Event, Native, SPEC_VERSION};
use crate::util::OrderedMap;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// One physical line of a JSONL file.
pub struct Line<'a> {
    /// Byte offset of the line's first byte.
    pub off: usize,
    /// The line without its terminating `\n`.
    pub text: &'a str,
    /// The parsed value, when the line is JSON.
    pub value: Option<Value>,
    /// `serde_json` re-serialises `value` to exactly `text`.
    pub exact: bool,
}

/// Split `text` into lines. Blank lines are skipped (they carry nothing and
/// no producer writes them); everything else, including torn or invalid
/// JSON, is returned so it can be kept verbatim.
pub fn lines(text: &str) -> Vec<Line<'_>> {
    let mut out = Vec::new();
    let mut off = 0usize;
    for piece in text.split_inclusive('\n') {
        let start = off;
        off += piece.len();
        let t = piece.strip_suffix('\n').unwrap_or(piece);
        if t.trim().is_empty() {
            continue;
        }
        let value: Option<Value> = serde_json::from_str(t).ok();
        let exact = value.as_ref().is_some_and(|v| serde_json::to_string(v).ok().as_deref() == Some(t));
        out.push(Line { off: start, text: t, value, exact });
    }
    out
}

/// The native record for a line, held as the line's own text (SPEC §4), with
/// `raw` too when a parsed value would re-spell it ([`hold`]). A line whose
/// parsed value is not what its text says (a stale carrier was taken out of
/// it) is held as that value, with the line kept as `raw`.
pub fn native_of(format: &str, variant: &str, version: Option<&str>, id: &str, l: &Line, file: Option<&str>) -> Native {
    let mut n = Native::new(format, Some(id.to_string())).variant(Some(variant)).version(version).file(file);
    match &l.value {
        Some(v) if serde_json::from_str::<Value>(l.text).ok().as_ref() == Some(v) => hold(&mut n, Some(l.text), v),
        Some(v) => {
            n.set_record(Some(v.clone()));
            n.raw = Some(l.text.to_string());
        }
        None => n.raw = Some(l.text.to_string()),
    }
    n
}

/// Hold a whole JSON file (or any JSON text that may span lines) as a native
/// record: the record is the text with the whitespace between tokens removed,
/// every token as written ([`minify`]), and `raw` is the text itself whenever
/// it is not already that. False, leaving `n` unchanged, if `text` is not JSON.
pub fn hold_file(n: &mut Native, text: &str) -> bool {
    // Removing whitespace can join two tokens into one (`1 2` → `12`): only JSON text is held.
    // And only text a `Value` can hold: a record is read back as one
    // (`Native::record_json`), and `1e400` or a lone `\ud800` is JSON syntax
    // no `Value` holds.
    if serde_json::from_str::<Value>(text).is_err() {
        return false;
    }
    let compact = minify(text);
    let Ok(held) = n.clone().record_raw(&compact) else { return false };
    *n = held;
    if compact != text || !crate::formats::atif::jsonfmt::value_round_trips(&compact) {
        n.raw = Some(text.to_string());
    }
    true
}

/// The exact line text a native part-0 record stands for.
/// Its `raw` while that still matches the record ([`crate::util::current_raw`]).
pub fn line_of(n: &Native) -> Option<String> {
    crate::util::native_text(n)
}

/// For every event, how many events the native record it starts stands for
/// in this ledger (part 0 plus the parts of the same id that follow it, up
/// to the next part 0 of that id). Sources may legitimately repeat an id.
pub fn part_counts(l: &Ledger, format: &str) -> Vec<usize> {
    let mut counts = vec![0usize; l.events.len()];
    let mut open: HashMap<String, usize> = HashMap::new();
    for (i, e) in l.events.iter().enumerate() {
        let Some(n) = e.native.as_ref().filter(|n| n.format == format) else { continue };
        let Some(id) = &n.id else { continue };
        if n.part == 0 {
            open.insert(id.clone(), i);
            counts[i] = 1;
        } else if let Some(&start) = open.get(id) {
            counts[start] += 1;
        }
    }
    counts
}

/// Attach a carrier holding `events` (and optionally the agent header) to an
/// object record ([`carry::attach_with`], so each carried native record's
/// exact text rides along). Nothing is written when there is nothing to carry.
pub fn attach(rec: &mut Value, events: &[Event], agent: Option<&Agent>) {
    carry::attach_with(rec, &[], events, agent);
}

/// A carrier for `events` over `subject`, a value the carrier is stored beside
/// rather than inside (a database row's other columns, a checkpoint's
/// writes): [`carry::attach_with`] on `{"subject": subject}`, so the carrier
/// keeps each carried record's exact text. `None` when there is nothing to carry.
#[cfg_attr(not(feature = "sqlite"), allow(dead_code))] // used by the SQLite-backed formats only
pub fn carrier_over(subject: &Value, events: &[Event], agent: Option<&Agent>) -> Option<Value> {
    let mut holder = json!({ "subject": subject });
    carry::attach_with(&mut holder, &[], events, agent);
    holder.as_object_mut()?.remove_ordered(carry::KEY)
}

/// What a [`carrier_over`] carrier holds, if `subject` is still what it was
/// made over ([`carry::take_with`]).
#[cfg_attr(not(feature = "sqlite"), allow(dead_code))] // used by the SQLite-backed formats only
pub fn carried_over(subject: &Value, carrier: &Value) -> Option<carry::Carried> {
    let mut holder = json!({ "subject": subject });
    holder[carry::KEY] = carrier.clone();
    carry::take_with(&mut holder, &[])
}

/// Attach an empty carrier: the record stands for no events of its own (it
/// renders facts carried elsewhere), so a restoring import adds nothing.
pub fn mark(rec: &mut Value) {
    let h = carry::record_hash(rec);
    if let Value::Object(m) = rec {
        m.insert(carry::KEY.to_string(), json!({"v": SPEC_VERSION, "h": h, "events": []}));
    }
}

/// Pretty JSON with two-space indent (JavaScript `JSON.stringify(v, null, 2)`).
pub fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).expect("json")
}

// `Restored` is the common case and is moved out at once; boxing it would
// change a public type for no measured gain.
#[allow(clippy::large_enum_variant)]
pub enum Taken {
    /// No carrier on the record.
    None,
    /// The record is unedited: these are the events it stands for.
    Restored(Vec<Event>, Option<Agent>),
    /// A carrier was present but the record changed since; it was removed.
    Stale,
}

/// Remove the carrier from `rec` and, when `restore`, return what it held
/// ([`carry::take_with`], which also puts back each carried record's exact text).
pub fn take(rec: &mut Value, restore: bool) -> Taken {
    if !rec.get(carry::KEY).is_some_and(Value::is_object) {
        return Taken::None;
    }
    if !restore {
        if let Value::Object(m) = rec {
            m.remove_ordered(carry::KEY);
        }
        return Taken::Stale;
    }
    match carry::take_with(rec, &[]) {
        Some(c) => Taken::Restored(c.events, c.agent),
        None => Taken::Stale,
    }
}

/// Every file under `dir` with extension `ext` (recursively), as
/// (relative path with `/` separators, absolute path), sorted.
pub fn walk(dir: &Path, ext: &str) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for e in walkdir::WalkDir::new(dir).sort_by_file_name().into_iter().filter_map(|e| e.ok()) {
        if !e.file_type().is_file() {
            continue;
        }
        let p = e.path();
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if name.starts_with('.') || !name.ends_with(ext) {
            continue;
        }
        let rel = p.strip_prefix(dir).unwrap_or(p).to_string_lossy().replace('\\', "/");
        out.push((rel, p.to_path_buf()));
    }
    out
}

/// An id usable verbatim as a native session id and file name.
pub fn is_safe_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) && !s.starts_with('.')
}

/// Text of a neutral tool output (string or parts array).
pub fn output_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// Test support: spellings a parsed `Value` does not keep, and what a ledger holds.
#[cfg(test)]
pub(crate) mod spelling {
    use crate::ledger::LedgerSet;
    use serde_json::Value;

    /// Object members a `Value` re-spells: key order (`b` before `a`) and
    /// `1.0`, `0.000005`, `1e3` (kept by a `Value` only under serde_json's
    /// `preserve_order` and `arbitrary_precision`, which protoledger never
    /// turns on),
    /// and the six-character escape `\u00e9` and the inner whitespace in
    /// `[1 ,2]` (kept by `Value` under no feature at all).
    pub(crate) const ODD: &str = r#""b":1,"a":1.0,"c":0.000005,"d":1e3,"g":"\u00e9", "f":[1 ,2]"#;

    /// Sentinel: a `Value` round trip re-spells `text` in this build, so a
    /// check that `text` came back cannot be passed by one.
    pub(crate) fn assert_value_loses(text: &str) {
        let v: Value = serde_json::from_str(text).expect("JSON");
        assert_ne!(serde_json::to_string(&v).unwrap(), text, "a Value round trip keeps this text; it guards nothing");
    }

    /// The text each part-0 native record of `format` is held as, in order.
    pub(crate) fn held(set: &LedgerSet, format: &str) -> Vec<String> {
        set.iter()
            .flat_map(|l| l.events.iter())
            .filter_map(|e| e.native.as_ref())
            .filter(|n| n.format == format && n.part == 0)
            .filter_map(|n| n.record_text().map(str::to_string))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Kind;

    #[test]
    fn lines_keep_offsets_and_exactness() {
        let t = "{\"a\":1}\n\n{ \"b\": 2 }\nnot json\n";
        let ls = lines(t);
        assert_eq!(ls.len(), 3);
        assert_eq!(ls[0].off, 0);
        assert!(ls[0].exact);
        assert_eq!(ls[1].off, 9);
        assert!(!ls[1].exact && ls[1].value.is_some());
        assert!(ls[2].value.is_none());
    }

    #[test]
    fn carrier_with_agent_round_trips() {
        let mut rec = json!({"type":"x"});
        let a = Agent { id: "a1".into(), created_at: None, source: None, parent: None, links: vec![], meta: Default::default() };
        attach(&mut rec, &[Event::new("e", Kind::Text)], Some(&a));
        match take(&mut rec, true) {
            Taken::Restored(evs, Some(ag)) => {
                assert_eq!(evs.len(), 1);
                assert_eq!(ag.id, "a1");
            }
            _ => panic!("not restored"),
        }
        assert_eq!(rec, json!({"type":"x"}));
    }

    /// A carried event whose native record is spelled as no `Value` spells it.
    fn odd_event() -> (Event, String) {
        let text = format!("{{{}}}", spelling::ODD);
        spelling::assert_value_loses(&text);
        let n = Native::new("x", Some("1".into())).record_raw(&text).unwrap();
        (Event::new("e", Kind::Native).native(n), text)
    }

    #[test]
    fn line_carrier_keeps_carried_record_text() {
        let (e, text) = odd_event();
        let mut rec = json!({"type":"x"});
        attach(&mut rec, &[e], None);
        // As a harness stores it: one line of text, read back.
        let mut rec: Value = serde_json::from_str(&serde_json::to_string(&rec).unwrap()).unwrap();
        match take(&mut rec, true) {
            Taken::Restored(evs, _) => assert_eq!(evs[0].native.as_ref().unwrap().record_text(), Some(text.as_str())),
            _ => panic!("not restored"),
        }
        // Opposed: an edited record is not restored.
        let (e, _) = odd_event();
        let mut rec = json!({"type":"x"});
        attach(&mut rec, &[e], None);
        rec["type"] = json!("y");
        assert!(matches!(take(&mut rec, true), Taken::Stale));
    }

    #[test]
    fn carrier_over_a_subject_keeps_carried_record_text() {
        let (e, text) = odd_event();
        let subject = json!({"id": 7, "role": "user"});
        let c = carrier_over(&subject, &[e], None).expect("a carrier");
        // As a database column holds it: text, read back.
        let c: Value = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        let back = carried_over(&subject, &c).expect("restored");
        assert_eq!(back.events[0].native.as_ref().unwrap().record_text(), Some(text.as_str()));
        // Opposed: a changed subject restores nothing.
        assert!(carried_over(&json!({"id": 7, "role": "assistant"}), &c).is_none());
        assert!(carrier_over(&subject, &[], None).is_none());
    }

    #[test]
    fn native_of_holds_the_line_as_written() {
        let line = format!("{{\"type\":\"message\",{}}}", spelling::ODD);
        spelling::assert_value_loses(&line);
        let ls = lines(&line);
        let n = native_of("x", "v", None, "a", &ls[0], None);
        assert_eq!(n.record_text(), Some(line.as_str()));
        assert_eq!(line_of(&n).as_deref(), Some(line.as_str()));
        let mut bare = n.clone();
        bare.raw = None;
        assert_eq!(line_of(&bare).as_deref(), Some(line.as_str()));
    }
}
