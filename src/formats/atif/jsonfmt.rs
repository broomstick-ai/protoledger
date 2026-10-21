//! Whole-file JSON formatting styles, so a format whose native unit is one
//! (often pretty-printed) JSON document can be reproduced byte for byte.
//!
//! The common producers are covered: Python `json.dumps` (with or without
//! `indent`, `ensure_ascii` on or off), JavaScript `JSON.stringify`, serde,
//! and PHP-style `json_encode` (escaped slashes, as Slack writes). A style is
//! detected by laying the document's own compact text out with each candidate
//! ([`Style::relay`], which copies every token byte for byte) and comparing the
//! bytes; a document no candidate reproduces is kept as raw text instead. An
//! unchanged document is rebuilt from its records' own texts (SPEC §4) and
//! laid out the same way, so key order, number spelling and escapes survive
//! whichever serde_json features are on; [`Style::render`] is for documents
//! that changed.

use crate::model::Native;
use crate::util::string_end;
pub(crate) use crate::util::{minify, value_round_trips};
use serde_json::{Map, Value, json};

/// Export loss: an unchanged document written from its parsed value, because
/// it could not be rebuilt from its own text (shared by the whole-file
/// exporters: atif, slack, gemini-cli session-json / checkpoint, copilot-cli
/// legacy-json).
pub(crate) const EXACT_TEXT_FALLBACK: &str = "exact-text-fallback";
pub(crate) const EXACT_TEXT_FALLBACK_DETAIL: &str = "an unchanged document could not be rebuilt from its records' own texts; it is written from their parsed values, so its spelling (layout, number forms, escapes) may differ from the source";

/// One way of laying a JSON document out as text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    /// Spaces per level, or `None` for a single line.
    pub indent: Option<u8>,
    /// Single-line only: `", "` and `": "` between items (Python's default)
    /// rather than `","` and `":"` (JavaScript, serde).
    pub spaced: bool,
    /// Escape every non-ASCII character as `\uXXXX` (Python `ensure_ascii`, PHP).
    pub ascii: bool,
    /// Escape `/` as `\/` (PHP `json_encode`, Slack).
    pub slash: bool,
    /// The document ends with a newline.
    pub newline: bool,
    /// Indented only: arrays of plain numbers on one line, `[1, 2, 3]`
    /// (Harbor's `format_trajectory_json`, used for token ids and logprobs).
    pub numbers_inline: bool,
}

impl Style {
    /// What protoledger writes when it has no native style to follow.
    pub const PRETTY: Style = Style { indent: Some(2), spaced: false, ascii: false, slash: false, newline: true, numbers_inline: false };
    /// Single-line JSON as JavaScript and serde write it.
    pub const COMPACT: Style = Style { indent: None, spaced: false, ascii: false, slash: false, newline: false, numbers_inline: false };

    pub fn render(&self, v: &Value) -> String {
        let mut out = String::new();
        self.write(&mut out, v, 0);
        if self.newline {
            out.push('\n');
        }
        out
    }

    fn write(&self, out: &mut String, v: &Value, depth: usize) {
        match v {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => out.push_str(&n.to_string()),
            Value::String(s) => write_str(out, s, self.ascii, self.slash),
            Value::Array(a) => {
                if a.is_empty() {
                    out.push_str("[]");
                    return;
                }
                if self.numbers_inline && self.indent.is_some() && a.iter().all(Value::is_number) {
                    out.push('[');
                    for (i, x) in a.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        self.write(out, x, depth);
                    }
                    out.push(']');
                    return;
                }
                out.push('[');
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        out.push_str(self.item_sep());
                    }
                    self.newline_indent(out, depth + 1);
                    self.write(out, x, depth + 1);
                }
                self.newline_indent(out, depth);
                out.push(']');
            }
            Value::Object(m) => {
                if m.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push('{');
                for (i, (k, x)) in m.iter().enumerate() {
                    if i > 0 {
                        out.push_str(self.item_sep());
                    }
                    self.newline_indent(out, depth + 1);
                    write_str(out, k, self.ascii, self.slash);
                    out.push_str(self.key_sep());
                    self.write(out, x, depth + 1);
                }
                self.newline_indent(out, depth);
                out.push('}');
            }
        }
    }

    fn item_sep(&self) -> &'static str {
        if self.indent.is_none() && self.spaced { ", " } else { "," }
    }

    fn key_sep(&self) -> &'static str {
        if self.indent.is_some() || self.spaced { ": " } else { ":" }
    }

    fn newline_indent(&self, out: &mut String, depth: usize) {
        if let Some(n) = self.indent {
            out.push('\n');
            for _ in 0..depth * n as usize {
                out.push(' ');
            }
        }
    }

    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("indent".into(), self.indent.map(|n| json!(n)).unwrap_or(Value::Null));
        if self.spaced {
            m.insert("spaced".into(), json!(true));
        }
        if self.ascii {
            m.insert("ascii".into(), json!(true));
        }
        if self.slash {
            m.insert("slash".into(), json!(true));
        }
        m.insert("newline".into(), json!(self.newline));
        if self.numbers_inline {
            m.insert("numbers_inline".into(), json!(true));
        }
        Value::Object(m)
    }

    pub fn from_value(v: &Value) -> Option<Style> {
        let o = v.as_object()?;
        let flag = |k: &str| o.get(k).and_then(Value::as_bool).unwrap_or(false);
        Some(Style {
            indent: o.get("indent").and_then(Value::as_u64).map(|n| n as u8),
            spaced: flag("spaced"),
            ascii: flag("ascii"),
            slash: flag("slash"),
            newline: flag("newline"),
            numbers_inline: flag("numbers_inline"),
        })
    }

    /// The style that lays `text` out, if any: the layout (indent, spacing,
    /// inline number arrays, final newline) is matched on the text's own
    /// tokens ([`Style::relay`]), so key order, number spelling and string
    /// escapes never decide it. `ascii` and `slash` (how strings this style
    /// renders anew are escaped) are read off the text.
    pub fn detect(text: &str, _parsed: &Value) -> Option<Style> {
        let newline = text.ends_with('\n');
        let body = text.strip_suffix('\n').unwrap_or(text);
        let compact = minify(body);
        // Indent guess from the first indented line.
        let guessed = body.lines().nth(1).map(|l| l.len() - l.trim_start_matches(' ').len()).filter(|n| *n > 0 && *n <= 8).map(|n| n as u8);
        let mut indents: Vec<Option<u8>> = Vec::new();
        if body.contains('\n') {
            indents.extend(guessed.map(Some));
            indents.extend([Some(2), Some(4)]);
        } else {
            indents.push(None);
        }
        let ascii = body.is_ascii();
        let slash = body.contains("\\/");
        for indent in indents {
            let spaced_opts: &[bool] = if indent.is_none() { &[false, true] } else { &[false] };
            let inline_opts: &[bool] = if indent.is_some() { &[false, true] } else { &[false] };
            for &spaced in spaced_opts {
                for &numbers_inline in inline_opts {
                    let s = Style { indent, spaced, ascii, slash, newline, numbers_inline };
                    if s.relay(&compact) == text {
                        return Some(s);
                    }
                }
            }
        }
        None
    }

    /// Compact JSON text laid out in this style, every token (string, number,
    /// literal) copied byte for byte: the text form of [`Style::render`], for
    /// documents rebuilt from records held as their source text.
    pub fn relay(&self, compact: &str) -> String {
        let b = compact.as_bytes();
        let mut out: Vec<u8> = Vec::with_capacity(b.len() * 2);
        let mut depth = 0usize;
        let newline_indent = |out: &mut Vec<u8>, depth: usize| {
            if let Some(n) = self.indent {
                out.push(b'\n');
                out.resize(out.len() + depth * n as usize, b' ');
            }
        };
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'"' => {
                    let end = string_end(b, i);
                    out.extend_from_slice(&b[i..end]);
                    i = end;
                    continue;
                }
                c @ (b'{' | b'[') => {
                    let close = if c == b'{' { b'}' } else { b']' };
                    if b.get(i + 1) == Some(&close) {
                        out.extend_from_slice(&[c, close]);
                        i += 2;
                        continue;
                    }
                    if c == b'[' && self.numbers_inline && self.indent.is_some() {
                        let end = value_end(b, i);
                        let inner = &compact[i + 1..end - 1];
                        if inner.split(',').all(is_number_token) {
                            out.push(b'[');
                            out.extend_from_slice(inner.replace(',', ", ").as_bytes());
                            out.push(b']');
                            i = end;
                            continue;
                        }
                    }
                    out.push(c);
                    depth += 1;
                    newline_indent(&mut out, depth);
                }
                c @ (b'}' | b']') => {
                    depth = depth.saturating_sub(1);
                    newline_indent(&mut out, depth);
                    out.push(c);
                }
                b',' => {
                    out.extend_from_slice(self.item_sep().as_bytes());
                    newline_indent(&mut out, depth);
                }
                b':' => out.extend_from_slice(self.key_sep().as_bytes()),
                c => out.push(c),
            }
            i += 1;
        }
        if self.newline {
            out.push(b'\n');
        }
        String::from_utf8(out).expect("adding ASCII whitespace keeps UTF-8")
    }
}

/// Hold a native record as its source text (SPEC §4), and also as `raw` when
/// a parsed value would re-spell that text: a carrier holds events as JSON
/// values, so only `raw` brings the spelling back through another harness.
/// A text a ledger line cannot hold (one with a line break) is kept as `raw`
/// beside the record held as `v`; with no text, the record is `v`.
pub(crate) fn hold(n: &mut Native, text: Option<&str>, v: &Value) {
    match text.map(|t| (t, n.clone().record_raw(t))) {
        Some((t, Ok(held))) => {
            *n = held;
            if !value_round_trips(t) {
                n.raw = Some(t.to_string());
            }
        }
        Some((t, Err(_))) => {
            n.set_record(Some(v.clone()));
            n.raw = Some(t.to_string());
        }
        None => n.set_record(Some(v.clone())),
    }
}

/// `set` with no `raw` beside any native record: what a ledger written by a
/// build whose parsed values reproduce the records' texts holds (it keeps no
/// `raw`). Exporting it must still give the source's bytes, from the records'
/// own texts alone.
#[cfg(test)]
pub(crate) fn without_raw(set: &crate::ledger::LedgerSet) -> crate::ledger::LedgerSet {
    let mut out = crate::ledger::LedgerSet::new();
    for l in set.iter() {
        let mut l = l.clone();
        for e in l.events.iter_mut() {
            if let Some(n) = e.native.as_mut() {
                n.raw = None;
            }
        }
        out.insert(l);
    }
    out
}

/// A number token (`-1.5e3`), as opposed to a string, literal or container.
fn is_number_token(t: &str) -> bool {
    let t = t.strip_prefix('-').unwrap_or(t);
    t.starts_with(|c: char| c.is_ascii_digit()) && t.bytes().all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-'))
}

/// The index just past the compact JSON value that starts at `b[start]`.
fn value_end(b: &[u8], start: usize) -> usize {
    match b.get(start) {
        Some(b'"') => string_end(b, start),
        Some(b'{' | b'[') => {
            let mut depth = 0usize;
            let mut i = start;
            while i < b.len() {
                match b[i] {
                    b'"' => {
                        i = string_end(b, i);
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return i + 1;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            b.len()
        }
        _ => {
            let mut i = start;
            while i < b.len() && !matches!(b[i], b',' | b']' | b'}') {
                i += 1;
            }
            i
        }
    }
}

/// The members of a compact JSON object's text, in order, as (key, the
/// member's value text). `None` if `compact` is not an object.
pub(crate) fn members(compact: &str) -> Option<Vec<(String, &str)>> {
    let b = compact.as_bytes();
    if b.first() != Some(&b'{') || b.last() != Some(&b'}') {
        return None;
    }
    let mut out = Vec::new();
    let mut i = 1;
    while i < b.len() - 1 {
        if b[i] != b'"' {
            return None;
        }
        let kend = string_end(b, i);
        let key: String = serde_json::from_str(&compact[i..kend]).ok()?;
        if b.get(kend) != Some(&b':') {
            return None;
        }
        let vend = value_end(b, kend + 1);
        out.push((key, &compact[kend + 1..vend]));
        i = vend;
        if b.get(i) == Some(&b',') {
            i += 1;
        }
    }
    Some(out)
}

/// The items of a compact JSON array's text, in order. `None` if `compact` is
/// not an array.
pub(crate) fn items(compact: &str) -> Option<Vec<&str>> {
    let b = compact.as_bytes();
    if b.first() != Some(&b'[') || b.last() != Some(&b']') {
        return None;
    }
    let mut out = Vec::new();
    let mut i = 1;
    while i < b.len() - 1 {
        let end = value_end(b, i);
        out.push(&compact[i..end]);
        i = end;
        if b.get(i) == Some(&b',') {
            i += 1;
        }
    }
    Some(out)
}

/// The value text of member `key` in a compact object's text.
pub(crate) fn member<'a>(compact: &'a str, key: &str) -> Option<&'a str> {
    members(compact)?.into_iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// A compact object's text with member `key`'s value replaced by `value`
/// (itself compact JSON text), in place. `None` if there is no such member.
pub(crate) fn with_member(compact: &str, key: &str, value: &str) -> Option<String> {
    let v = member(compact, key)?;
    let start = v.as_ptr() as usize - compact.as_ptr() as usize;
    Some(format!("{}{}{}", &compact[..start], value, &compact[start + v.len()..]))
}

/// The compact text of an array holding `items` (each compact JSON text).
pub(crate) fn array_text<'a>(items: impl IntoIterator<Item = &'a str>) -> String {
    let mut s = String::from("[");
    for (i, x) in items.into_iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(x);
    }
    s.push(']');
    s
}

fn write_str(out: &mut String, s: &str, ascii: bool, slash: bool) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '/' if slash => out.push_str("\\/"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if ascii && (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{:04x}", unit));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detect(text: &str) -> Option<Style> {
        Style::detect(text, &serde_json::from_str(text).unwrap())
    }

    #[test]
    fn python_indent_ascii() {
        let text = "{\n  \"a\": \"caf\\u00e9 \\ud83d\\ude00\",\n  \"b\": [],\n  \"c\": [\n    1.50,\n    {}\n  ]\n}";
        let s = detect(text).unwrap();
        assert_eq!(s.indent, Some(2));
        assert!(s.ascii);
        assert!(!s.newline);
    }

    #[test]
    fn python_compact_and_js_compact() {
        let s = detect("{\"a\": 1, \"b\": [1, 2]}").unwrap();
        assert!(s.spaced);
        let s = detect("{\"a\":1,\"b\":[1,2],\"c\":\"é\"}\n").unwrap();
        assert!(!s.spaced && !s.ascii && s.newline);
    }

    #[test]
    fn php_slashes_four_spaces() {
        let text = "[\n    {\n        \"url\": \"https:\\/\\/x.test\\/a\"\n    }\n]";
        let s = detect(text).unwrap();
        assert_eq!(s.indent, Some(4));
        assert!(s.slash);
    }

    #[test]
    fn harbor_inline_numeric_arrays() {
        let text = "{\n  \"ids\": [1, 2, -3.5e-05],\n  \"nested\": [\n    [1],\n    \"x\"\n  ]\n}";
        let s = detect(text).unwrap();
        assert!(s.numbers_inline);
    }

    /// Layouts are read off the text's own tokens, so a document a parsed
    /// `Value` would re-spell under any feature set (keys out of order, the
    /// escape `\u00e9` beside a raw `ü`, `1e3`, `0.000005`, `\/`) is detected
    /// and laid out again byte for byte from its compact text.
    #[test]
    fn detect_and_relay_keep_every_token() {
        let texts = [
            // two-space indent, number arrays inline (Harbor), final newline
            "{\n  \"z\": \"caf\\u00e9 \u{fc}\",\n  \"ids\": [1e3, 0.000005, -2],\n  \"a\": {\n    \"n\": 1.50\n  }\n}\n",
            // Python's spaced single line
            "{\"z\": \"caf\\u00e9 \u{fc}\", \"a\": [1e3, {\"b\": 0.000005}]}",
            // PHP: four spaces, escaped slashes
            "[\n    {\n        \"url\": \"https:\\/\\/x.test\\/caf\\u00e9\",\n        \"n\": 1e3\n    }\n]",
        ];
        for text in texts {
            let s = detect(text).unwrap_or_else(|| panic!("no layout found for {text:?}"));
            assert_eq!(s.relay(&minify(text)), text);
        }
        assert!(detect(texts[0]).unwrap().numbers_inline);
        assert!(detect(texts[1]).unwrap().spaced);
        let php = detect(texts[2]).unwrap();
        assert!(php.slash && php.ascii && php.indent == Some(4));
        // Opposed: a layout no style lays out is still refused.
        assert!(detect("{\n  \"a\": 1e3,\n    \"b\": 2\n}").is_none());
    }

    #[test]
    fn members_and_items_cut_compact_text() {
        let c = "{\"b\":[1,{\"x\":\"a,]}\\\"\"}],\"a\":1e3,\"c\":{}}";
        let m = members(c).unwrap();
        assert_eq!(m, [("b".to_string(), "[1,{\"x\":\"a,]}\\\"\"}]"), ("a".to_string(), "1e3"), ("c".to_string(), "{}")]);
        assert_eq!(items(m[0].1).unwrap(), ["1", "{\"x\":\"a,]}\\\"\"}"]);
        assert_eq!(items("[]").unwrap(), Vec::<&str>::new());
        assert_eq!(with_member(c, "a", "[]").unwrap(), "{\"b\":[1,{\"x\":\"a,]}\\\"\"}],\"a\":[],\"c\":{}}");
        assert!(with_member(c, "nope", "1").is_none());
        assert!(members("[1]").is_none() && items("{}").is_none());
        assert_eq!(array_text(["1e3", "\"x\""]), "[1e3,\"x\"]");
    }

    #[test]
    fn unknown_layout_is_none() {
        assert!(detect("{ \"a\": 1 }").is_none());
        assert!(detect("{\n  \"a\": [ 1, 2 ]\n}").is_none());
    }

    #[test]
    fn style_round_trips_through_json() {
        let s = Style { indent: Some(4), spaced: false, ascii: true, slash: true, newline: true, numbers_inline: true };
        assert_eq!(Style::from_value(&s.to_value()), Some(s));
        assert_eq!(Style::from_value(&Style::COMPACT.to_value()), Some(Style::COMPACT));
    }
}
