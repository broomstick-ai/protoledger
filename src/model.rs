//! The protoledger data model (SPEC.md §2-§5).
//!
//! Bodies and metadata are kept as JSON maps rather than closed structs, so a
//! reader preserves every field it does not understand (SPEC §2.2, §8).

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{Map, Value};

pub const SPEC_VERSION: &str = "0.1";

pub type Obj = Map<String, Value>;

/// Line 1 of a ledger file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Header {
    pub protoledger: String,
    pub agent: Agent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<Parent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
    #[serde(default, skip_serializing_if = "Obj::is_empty")]
    pub meta: Obj,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_id: Option<String>,
    /// Format-specific facts about the source container that an exporter needs
    /// to rebuild it exactly (file name, directory slug, ...).
    #[serde(default, skip_serializing_if = "Obj::is_empty")]
    pub extra: Obj,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Relation {
    Subagent,
    Fork,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Parent {
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    pub relation: Relation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Link {
    pub rel: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    #[serde(default, skip_serializing_if = "Obj::is_empty")]
    pub extra: Obj,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Text,
    Media,
    Reasoning,
    ToolCall,
    ToolResult,
    Compaction,
    Turn,
    Usage,
    Settings,
    Title,
    Subagent,
    PeerMessage,
    FileChange,
    Error,
    App,
    Native,
}

impl Kind {
    pub const ALL: [Kind; 16] = [
        Kind::Text,
        Kind::Media,
        Kind::Reasoning,
        Kind::ToolCall,
        Kind::ToolResult,
        Kind::Compaction,
        Kind::Turn,
        Kind::Usage,
        Kind::Settings,
        Kind::Title,
        Kind::Subagent,
        Kind::PeerMessage,
        Kind::FileChange,
        Kind::Error,
        Kind::App,
        Kind::Native,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Media => "media",
            Kind::Reasoning => "reasoning",
            Kind::ToolCall => "tool_call",
            Kind::ToolResult => "tool_result",
            Kind::Compaction => "compaction",
            Kind::Turn => "turn",
            Kind::Usage => "usage",
            Kind::Settings => "settings",
            Kind::Title => "title",
            Kind::Subagent => "subagent",
            Kind::PeerMessage => "peer_message",
            Kind::FileChange => "file_change",
            Kind::Error => "error",
            Kind::App => "app",
            Kind::Native => "native",
        }
    }

    /// SPEC §3: content kinds default to the model's audience.
    pub fn default_audience(self, body: &Obj) -> Audience {
        match self {
            Kind::Text | Kind::Media | Kind::Reasoning | Kind::ToolCall | Kind::ToolResult | Kind::PeerMessage => Audience::Model,
            Kind::Compaction if body.get("phase").and_then(Value::as_str) == Some("summary") => Audience::Model,
            _ => Audience::Record,
        }
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Audience {
    Model,
    Record,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Native {
    pub format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The native record's id (or location anchor). Shared by every event made
    /// from the same record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Index of this event among the events made from one native record.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub part: u32,
    /// The whole native record, on part 0 only, held as its exact JSON text
    /// (SPEC §4). Read it with [`Native::record_text`] (exact) or
    /// [`Native::record_json`] (parsed); set it with [`Native::record`],
    /// [`Native::record_raw`] or [`Native::set_record`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<Box<RawValue>>,
    /// Exact source text, when `record` would not re-serialise to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    /// Source file, relative to the source set root, when one agent spans files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// The record was replaced by a scrubbed one ([`redact`]): it is no longer
    /// the source's bytes, so a trip through it is not expected to be exact.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub altered: bool,
}

/// Two natives are equal when their records are equal as JSON values (the
/// equality `record` had while it was a `Value`), not as text.
impl PartialEq for Native {
    fn eq(&self, o: &Native) -> bool {
        self.format == o.format
            && self.variant == o.variant
            && self.version == o.version
            && self.id == o.id
            && self.part == o.part
            && self.raw == o.raw
            && self.file == o.file
            && self.altered == o.altered
            && (self.record_text() == o.record_text() || self.record_json() == o.record_json())
    }
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "EventWire", into = "EventWire")]
pub struct Event {
    pub id: String,
    pub seq: u64,
    pub ts: Option<String>,
    pub kind: Kind,
    /// The kind as written, when a reader did not know it (SPEC §2.2): the
    /// event is handled as `native` and written back under its own name.
    pub unknown_kind: Option<String>,
    pub turn: Option<String>,
    pub group: Option<String>,
    pub after: Option<String>,
    pub audience: Option<Audience>,
    pub body: Obj,
    pub meta: Obj,
    pub native: Option<Native>,
}

/// The on-disk spelling of an event, with `kind` as free text.
#[derive(Serialize, Deserialize)]
struct EventWire {
    id: String,
    seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ts: Option<String>,
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    audience: Option<Audience>,
    #[serde(default)]
    body: Obj,
    #[serde(default, skip_serializing_if = "Obj::is_empty")]
    meta: Obj,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    native: Option<Native>,
}

impl From<EventWire> for Event {
    fn from(w: EventWire) -> Event {
        let known = Kind::ALL.iter().copied().find(|k| k.as_str() == w.kind);
        Event {
            id: w.id,
            seq: w.seq,
            ts: w.ts,
            kind: known.unwrap_or(Kind::Native),
            unknown_kind: if known.is_none() { Some(w.kind) } else { None },
            turn: w.turn,
            group: w.group,
            after: w.after,
            audience: w.audience,
            body: w.body,
            meta: w.meta,
            native: w.native,
        }
    }
}

impl From<Event> for EventWire {
    fn from(e: Event) -> EventWire {
        EventWire {
            id: e.id,
            seq: e.seq,
            ts: e.ts,
            kind: e.unknown_kind.unwrap_or_else(|| e.kind.as_str().to_string()),
            turn: e.turn,
            group: e.group,
            after: e.after,
            audience: e.audience,
            body: e.body,
            meta: e.meta,
            native: e.native,
        }
    }
}

impl Event {
    /// A new event; `seq` is assigned when it is appended to a ledger.
    pub fn new(id: impl Into<String>, kind: Kind) -> Self {
        Event {
            id: id.into(),
            seq: 0,
            ts: None,
            kind,
            unknown_kind: None,
            turn: None,
            group: None,
            after: None,
            audience: None,
            body: Obj::new(),
            meta: Obj::new(),
            native: None,
        }
    }

    pub fn ts(mut self, ts: Option<impl Into<String>>) -> Self {
        self.ts = ts.map(Into::into);
        self
    }
    pub fn turn(mut self, turn: Option<impl Into<String>>) -> Self {
        self.turn = turn.map(Into::into);
        self
    }
    pub fn group(mut self, group: Option<impl Into<String>>) -> Self {
        self.group = group.map(Into::into);
        self
    }
    pub fn after(mut self, after: Option<impl Into<String>>) -> Self {
        self.after = after.map(Into::into);
        self
    }
    pub fn audience(mut self, a: Audience) -> Self {
        self.audience = Some(a);
        self
    }
    /// Set a body field; `Value::Null` values are skipped.
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        let v = value.into();
        if !v.is_null() {
            self.body.insert(key.to_string(), v);
        }
        self
    }
    pub fn meta_ns(mut self, ns: &str, value: Value) -> Self {
        self.meta.insert(ns.to_string(), value);
        self
    }
    pub fn native(mut self, n: Native) -> Self {
        self.native = Some(n);
        self
    }

    /// The audience, falling back to the kind's default.
    pub fn effective_audience(&self) -> Audience {
        self.audience.unwrap_or_else(|| self.kind.default_audience(&self.body))
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        self.body.get(key).and_then(Value::as_str)
    }
}

impl Native {
    pub fn new(format: &str, id: Option<String>) -> Self {
        Native { format: format.to_string(), variant: None, version: None, id, part: 0, record: None, raw: None, file: None, altered: false }
    }
    pub fn variant(mut self, v: Option<impl Into<String>>) -> Self {
        self.variant = v.map(Into::into);
        self
    }
    pub fn version(mut self, v: Option<impl Into<String>>) -> Self {
        self.version = v.map(Into::into);
        self
    }
    /// Set the record from a parsed value; it is held as `r`'s compact
    /// serialisation, which is the source's bytes only if `r` re-serialises to them.
    pub fn record(mut self, r: Value) -> Self {
        self.set_record(Some(r));
        self
    }
    /// Set the record from its exact source text, kept byte for byte. Fails if
    /// `text` is not one JSON value, if it holds a line break (a ledger line
    /// cannot carry one), or if a [`Value`] cannot hold it (a number past
    /// `f64`, a lone surrogate): [`RawValue`] checks syntax only, and
    /// [`Native::record_json`] must be able to parse every record held.
    pub fn record_raw(mut self, text: &str) -> serde_json::Result<Self> {
        if text.contains(['\n', '\r']) {
            return Err(serde::de::Error::custom("a native record held as text cannot contain a line break"));
        }
        let raw = RawValue::from_string(text.to_string())?;
        serde_json::from_str::<Value>(raw.get())?;
        self.record = Some(raw);
        Ok(self)
    }
    /// Replace (or clear) the record with a parsed value's compact serialisation.
    pub fn set_record(&mut self, r: Option<Value>) {
        self.record = r.map(|r| serde_json::value::to_raw_value(&r).expect("a JSON value serialises"));
    }
    /// The record parsed. A parsed `Value` sorts keys and re-spells numbers
    /// (protoledger builds serde_json without `preserve_order` and
    /// `arbitrary_precision`); use [`Native::record_text`] for the exact bytes.
    pub fn record_json(&self) -> Option<Value> {
        self.record.as_ref().map(|r| serde_json::from_str(r.get()).expect("a native record is valid JSON"))
    }
    /// The record exactly as held.
    pub fn record_text(&self) -> Option<&str> {
        self.record.as_deref().map(RawValue::get)
    }
    /// This native with its record scrubbed by `f` and marked `altered` (see [`redact`]).
    pub fn redacted(mut self, f: impl FnOnce(&mut Value)) -> Native {
        redact(&mut self, f);
        self
    }
    pub fn raw(mut self, r: impl Into<String>) -> Self {
        self.raw = Some(r.into());
        self
    }
    pub fn file(mut self, f: Option<impl Into<String>>) -> Self {
        self.file = f.map(Into::into);
        self
    }
    /// A part > 0 of the same native record: no record copy.
    pub fn part_of(&self, part: u32) -> Native {
        Native { part, record: None, raw: None, ..self.clone() }
    }
}

/// Replace `native`'s record with a scrubbed one: `f` edits the parsed record
/// (or the parsed `raw` text when there is no record), the result becomes the
/// record, `raw` is dropped (it would still hold what was scrubbed), and the
/// native is marked `altered` (SPEC §4). Returns false, leaving the native
/// unchanged, when it has no record and no `raw` that parses as JSON.
pub fn redact(native: &mut Native, f: impl FnOnce(&mut Value)) -> bool {
    let parsed = native.record_json().or_else(|| native.raw.as_deref().and_then(|r| serde_json::from_str(r).ok()));
    let Some(mut v) = parsed else { return false };
    f(&mut v);
    native.set_record(Some(v));
    native.raw = None;
    native.altered = true;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Ledger;
    use serde_json::json;

    /// Source spellings that a parsed `Value` does not keep: key order and
    /// number spelling (kept by a `Value` only if serde_json's `preserve_order`
    /// and `arbitrary_precision` were on, which protoledger never turns on), and the
    /// six-character escape `\u00e9` under `"g"` and inner whitespace (kept by
    /// `Value` under no feature at all; `"e"` is the literal UTF-8 `é`, for contrast).
    const ODD: &str = r#"{"b":1,"a":1.0,"c":0.000005,"d":1e3,"e":"é","g":"\u00e9", "f":[1 ,2]}"#;

    fn ledger_with(n: Native) -> Ledger {
        let mut l = Ledger::new(Agent { id: "a".into(), created_at: None, source: None, parent: None, links: vec![], meta: Obj::new() });
        l.append(Event::new("e1", Kind::Native).native(n)).unwrap();
        l
    }

    #[test]
    fn record_raw_reemits_exactly_through_a_ledger_line() {
        for src in [r#"{"b":1,"a":1.0}"#, ODD] {
            let n = Native::new("x", Some("1".into())).record_raw(src).unwrap();
            assert_eq!(n.record_text(), Some(src));
            let text = ledger_with(n).to_jsonl();
            assert!(text.contains(src), "ledger line does not hold the record verbatim: {text}");
            let (back, _) = Ledger::from_jsonl(&text).unwrap();
            assert_eq!(back.events[0].native.as_ref().unwrap().record_text(), Some(src));
            // Writing what was read changes nothing.
            assert_eq!(back.to_jsonl(), text);
        }
        // The escape is still the six characters `\u00e9`, not the `é` it decodes to.
        let n = Native::new("x", None).record_raw(ODD).unwrap();
        let (back, _) = Ledger::from_jsonl(&ledger_with(n).to_jsonl()).unwrap();
        let held = back.events[0].native.as_ref().unwrap().record_text().unwrap().to_string();
        assert!(held.contains(r#""g":"\u00e9""#), "the escape was not kept: {held}");
        assert_eq!(back.events[0].native.as_ref().unwrap().record_json().unwrap()["g"], json!("é"));
    }

    #[test]
    fn record_built_from_a_value_is_not_the_source_spelling() {
        // The opposite case: a record set from a parsed Value is that Value's
        // compact text, so a differently spelled source is NOT reproduced.
        let v: Value = serde_json::from_str(ODD).unwrap();
        let n = Native::new("x", None).record(v.clone());
        assert_ne!(n.record_text(), Some(ODD));
        assert_eq!(n.record_text(), Some(serde_json::to_string(&v).unwrap().as_str()));
        // ... and both spellings parse to the same JSON.
        let raw = Native::new("x", None).record_raw(ODD).unwrap();
        assert_eq!(raw.record_json(), n.record_json());
        assert_eq!(raw, n, "Native equality is JSON equality of the record");
        assert_ne!(raw, Native::new("x", None).record(json!({"b": 2})));
    }

    #[test]
    fn record_raw_refuses_what_a_ledger_line_cannot_hold() {
        assert!(Native::new("x", None).record_raw("{\"a\":\n1}").is_err());
        assert!(Native::new("x", None).record_raw("{\"a\":").is_err());
        assert!(Native::new("x", None).record_raw("{\"a\": 1}").is_ok());
        // Text a `Value` cannot hold is refused, so `record_json`
        // never meets it (it panicked on both). Opposed by the
        // same shapes in range, held, and read back.
        for bad in [r#"{"a":1e400}"#, r#"{"a":"\ud800"}"#, r#"[-1e309]"#] {
            assert!(Native::new("x", None).record_raw(bad).is_err(), "{bad} was held");
        }
        for good in [r#"{"a":1e300}"#, r#"{"a":"\ud83d\ude00"}"#, r#"[-1e308]"#] {
            let n = Native::new("x", None).record_raw(good).unwrap();
            assert_eq!(n.record_text(), Some(good));
            assert!(n.record_json().is_some(), "{good}");
        }
    }

    #[test]
    fn redact_replaces_the_record_and_marks_it_altered() {
        let src = r#"{"user":"me","token":"s3cret","n":1.0}"#;
        let n = Native::new("x", Some("1".into())).record_raw(src).unwrap().raw(src);
        assert!(!n.altered);
        let r = n.clone().redacted(|v| {
            v["token"] = json!("[redacted]");
        });
        assert!(r.altered, "a redacted record must be marked altered");
        assert!(!r.record_text().unwrap().contains("s3cret"));
        assert_eq!(r.raw, None, "raw would still hold the scrubbed text");
        assert_eq!(r.record_json().unwrap()["token"], json!("[redacted]"));
        // The mark survives a ledger line; an unredacted native carries none.
        let text = ledger_with(r).to_jsonl();
        assert!(text.contains(r#""altered":true"#), "{text}");
        let (back, _) = Ledger::from_jsonl(&text).unwrap();
        assert!(back.events[0].native.as_ref().unwrap().altered);
        let plain = ledger_with(n).to_jsonl();
        assert!(!plain.contains("altered"), "{plain}");
        let (back, _) = Ledger::from_jsonl(&plain).unwrap();
        assert!(!back.events[0].native.as_ref().unwrap().altered);
    }

    #[test]
    fn redact_without_a_record_changes_nothing() {
        let mut n = Native::new("x", Some("1".into()));
        assert!(!redact(&mut n, |v| *v = json!({})));
        assert!(!n.altered);
        assert_eq!(n.record_text(), None);
        // A raw-only native is redacted from its parsed raw text.
        let mut n = Native::new("x", None).raw(r#"{"k":"v"}"#);
        assert!(redact(&mut n, |v| v["k"] = json!("w")));
        assert!(n.altered);
        assert_eq!((n.raw.as_deref(), n.record_text()), (None, Some(r#"{"k":"w"}"#)));
    }
}
