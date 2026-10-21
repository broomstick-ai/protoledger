//! Bindings as ledger facts (SPEC §9.2, §9.3).
//!
//! A binding is recorded in the ledger itself, as `app` events:
//!
//! - `protoledger/bound`    `{binding, format, variant, location, native_id, mode}`
//! - `protoledger/unbound`  `{binding}`
//! - `protoledger/diverged` `{binding, agent, detail}`: sync stopped following it
//! - `protoledger/projected` `{binding, bytes, sha256, through}` (SPEC §11.2):
//!   the bytes protoledger wrote to a projection binding's location file,
//!   which sync measures the file against ([`Projections`]). It is a §11
//!   fact, so no projected view holds it; it does not change the binding row.
//!
//! The index's `bindings` table is rebuilt from these events whenever a ledger
//! is indexed (append, catch-up and reindex alike), so it is derived like every
//! other table. The only column a ledger does not say is the sync `cursor`.

use crate::model::{Event, Kind};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const BOUND: &str = "protoledger/bound";
pub const UNBOUND: &str = "protoledger/unbound";
pub const DIVERGED: &str = "protoledger/diverged";
pub const PROJECTED: &str = "protoledger/projected";

/// How a binding came to be (SPEC §9.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// The ledger was exported to the location; what the harness appends comes back.
    Projection,
    /// The native session came first; it was imported and is followed.
    Attachment,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Projection => "projection",
            Mode::Attachment => "attachment",
        }
    }
}

/// The data of a `protoledger/bound` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bound {
    pub binding: String,
    pub format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    pub location: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_id: Option<String>,
    pub mode: Mode,
}

/// A binding fact read from one event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fact {
    Bound(Bound),
    Unbound { binding: String },
    Diverged { binding: String, detail: String },
}

fn app(id: String, ty: &str, data: Value) -> Event {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    Event::new(id, Kind::App).ts(Some(crate::util::ms_to_rfc3339(now))).with("type", ty).with("data", data)
}

pub fn bound_event(b: &Bound) -> Event {
    app(format!("{BOUND}:{}", b.binding), BOUND, serde_json::to_value(b).expect("a binding serialises"))
}

pub fn unbound_event(binding: &str) -> Event {
    app(format!("{UNBOUND}:{binding}"), UNBOUND, json!({ "binding": binding }))
}

pub fn diverged_event(binding: &str, agent: &str, detail: &str) -> Event {
    app(format!("{DIVERGED}:{binding}"), DIVERGED, json!({ "binding": binding, "agent": agent, "detail": detail }))
}

/// The binding fact `e` records, if it is one.
pub fn fact(e: &Event) -> Option<Fact> {
    if e.kind != Kind::App || e.unknown_kind.is_some() {
        return None;
    }
    let data = e.body.get("data")?;
    let binding = || data.get("binding").and_then(Value::as_str).map(str::to_string);
    match e.str("type")? {
        BOUND => serde_json::from_value(data.clone()).ok().map(Fact::Bound),
        UNBOUND => Some(Fact::Unbound { binding: binding()? }),
        DIVERGED => Some(Fact::Diverged { binding: binding()?, detail: data.get("detail").and_then(Value::as_str).unwrap_or_default().to_string() }),
        _ => None,
    }
}

/// The data of a `protoledger/projected` fact (SPEC §11.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Projected {
    pub binding: String,
    /// The length of the render of the binding's location file.
    pub bytes: u64,
    /// The lowercase hex SHA-256 of those bytes.
    pub sha256: String,
    /// The last event of the bound agent's projected view; `None` (`null`) when it holds none.
    pub through: Option<String>,
}

impl Projected {
    /// The fact for `bytes` (the render of a location file) and `through`.
    pub fn of(binding: &str, bytes: &[u8], through: Option<String>) -> Projected {
        Projected { binding: binding.to_string(), bytes: bytes.len() as u64, sha256: sha256_hex(bytes), through }
    }
    /// Does this fact name the same render as `other` (the step-6 test: one
    /// fact per change)?
    pub fn same(&self, other: &Projected) -> bool {
        (self.bytes, &self.sha256, &self.through) == (other.bytes, &other.sha256, &other.through)
    }
    /// Does `file` begin with this fact's bytes (SPEC §11.2, "matches")?
    pub fn matches(&self, file: &[u8]) -> bool {
        file.len() as u64 >= self.bytes && sha256_hex(&file[..self.bytes as usize]) == self.sha256
    }
}

/// Lowercase hex SHA-256.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

pub fn projected_event(p: &Projected) -> Event {
    app(format!("{PROJECTED}:{}", uuid::Uuid::new_v4()), PROJECTED, serde_json::to_value(p).expect("a projected fact serialises"))
}

/// One `protoledger/projected` fact of a binding, where it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedFact {
    /// The fact's event id.
    pub id: String,
    pub seq: u64,
    pub data: Projected,
}

/// A projection binding's `protoledger/projected` facts, read from its
/// agent's ledger (SPEC §11.2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Projections {
    /// In `seq` order. A fact that stands before the binding's `bound`
    /// fact, or whose `data` is malformed, is one its writer would have
    /// refused (SPEC §11) and is not here.
    pub facts: Vec<ProjectedFact>,
    /// The binding was made without a `protoledger/projected` fact (before
    /// SPEC §11.2, or by a `project` that stopped between its two appends):
    /// the event right after its `bound` fact is not one of its facts.
    pub legacy: bool,
}

impl Projections {
    /// The binding's latest fact (highest `seq`).
    pub fn last(&self) -> Option<&ProjectedFact> {
        self.facts.last()
    }
    /// The latest fact that `file` matches (SPEC §11.2).
    pub fn matching(&self, file: &[u8]) -> Option<&ProjectedFact> {
        self.facts.iter().rev().find(|f| f.data.matches(file))
    }
}

/// The `protoledger/projected` facts of `binding` among `events` (one
/// agent's ledger, in order).
pub fn projections<'a>(events: impl IntoIterator<Item = &'a Event>, binding: &str) -> Projections {
    let events: Vec<&Event> = events.into_iter().collect();
    let Some(at) = events.iter().position(|e| matches!(fact(e), Some(Fact::Bound(b)) if b.binding == binding)) else {
        return Projections::default();
    };
    let facts: Vec<ProjectedFact> = events[at + 1..]
        .iter()
        .filter_map(|e| projected_fact(e).filter(|p| p.binding == binding).map(|data| ProjectedFact { id: e.id.clone(), seq: e.seq, data }))
        .collect();
    let first = events.get(at + 1).and_then(|e| projected_fact(e)).filter(|p| p.binding == binding);
    Projections { facts, legacy: first.is_none() }
}

/// The `protoledger/projected` fact `e` records, if it is one.
pub fn projected_fact(e: &Event) -> Option<Projected> {
    if e.kind != Kind::App || e.unknown_kind.is_some() || e.str("type")? != PROJECTED {
        return None;
    }
    let p: Projected = serde_json::from_value(e.body.get("data")?.clone()).ok()?;
    (p.sha256.len() == 64 && p.sha256.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())).then_some(p)
}
