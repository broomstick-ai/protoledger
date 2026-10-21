//! Message delivery (SPEC §11.3): the four facts an application appends to
//! record a message it queued for an agent and what became of it, and the
//! delivery state derived from them.
//!
//! - [`message`] appends `protoledger/message {message_id, text, meta}` when a
//!   message is queued;
//! - [`delivered`] appends `protoledger/delivered {message_ids, turn?, event?}`
//!   when the messages reached the model;
//! - [`cancelled`] appends `protoledger/cancelled {message_ids, reason}` when
//!   one attempt to deliver them did not send them (an interrupt, a failed
//!   turn): a cancelled message is **still owed**;
//! - [`withdrawn`] appends `protoledger/withdrawn {message_ids, reason}` when
//!   the sender retracts them: the only fact that makes a message no longer
//!   owed without delivering it.
//!
//! Each is checked against the ledger as its one writer ([`Appender`]) holds
//! it, under that writer's lock, and a refused one ([`DeliveryError`])
//! appends nothing. [`state`] derives every message's [`State`] and history,
//! and the should-send set (queued − delivered − withdrawn), from the facts
//! alone, in `seq` order, ignoring (and reporting) any fact the writer would
//! have refused where it stands. Nothing is rewritten: a resend is a new
//! `protoledger/delivered` fact, and protoledger never resends anything
//! itself; the application acts on [`Delivery::should_send`].
//!
//! Retries, parks, budgets and account switches are an application's own
//! operational facts and are not modelled here (SPEC §11).

use crate::ledger::{Appended, Appender, Ledger, fact_type};
use crate::model::{Event, Kind, Obj};
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

/// `protoledger/message {message_id, text, meta}` (SPEC §11.3).
pub const MESSAGE: &str = "protoledger/message";
/// `protoledger/delivered {message_ids, turn?, event?}` (SPEC §11.3).
pub const DELIVERED: &str = "protoledger/delivered";
/// `protoledger/cancelled {message_ids, reason}` (SPEC §11.3).
pub const CANCELLED: &str = "protoledger/cancelled";
/// `protoledger/withdrawn {message_ids, reason}` (SPEC §11.3).
pub const WITHDRAWN: &str = "protoledger/withdrawn";

/// A message's delivery state (SPEC §11.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum State {
    /// Queued; no fact names it yet. Owed.
    Pending,
    /// Its latest fact is a `protoledger/cancelled`: one attempt did not send
    /// it. **Still owed** (SPEC §11.3).
    Cancelled,
    /// A `protoledger/delivered` lists it. Sent; final.
    Delivered,
    /// A `protoledger/withdrawn` lists it. Not owed; final.
    Withdrawn,
}

impl State {
    /// Is a message in this state still owed (should send)?
    pub fn owed(self) -> bool {
        matches!(self, State::Pending | State::Cancelled)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            State::Pending => "pending",
            State::Cancelled => "cancelled",
            State::Delivered => "delivered",
            State::Withdrawn => "withdrawn",
        }
    }
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The three facts that move a message out of `pending` (SPEC §11.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Act {
    Delivered,
    Cancelled,
    Withdrawn,
}

impl Act {
    /// The fact's `app` type.
    pub fn fact(self) -> &'static str {
        match self {
            Act::Delivered => DELIVERED,
            Act::Cancelled => CANCELLED,
            Act::Withdrawn => WITHDRAWN,
        }
    }

    fn of(ty: &str) -> Option<Act> {
        match ty {
            DELIVERED => Some(Act::Delivered),
            CANCELLED => Some(Act::Cancelled),
            WITHDRAWN => Some(Act::Withdrawn),
            _ => None,
        }
    }

    /// The state this act moves a message to.
    pub fn to(self) -> State {
        match self {
            Act::Delivered => State::Delivered,
            Act::Cancelled => State::Cancelled,
            Act::Withdrawn => State::Withdrawn,
        }
    }
}

/// Why a delivery fact is refused (SPEC §11.3). A refused one appends
/// nothing; a reader ignores a fact of this shape it finds in a ledger.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeliveryError {
    #[error("message {0:?} is already in the ledger (a protoledger/message names it)")]
    Duplicate(String),
    #[error("{0} lists no messages")]
    Empty(&'static str),
    #[error("{0} lists message {1:?} twice")]
    Repeated(&'static str, String),
    #[error("{0} names message {1:?}, which no protoledger/message in the ledger names")]
    Unknown(&'static str, String),
    #[error("{0} names message {1:?}, which is {2}: a {0} may not leave that state")]
    Final(&'static str, String, State),
    /// A fact found in a ledger whose `data` is not of its type's shape.
    /// The writer never writes one.
    #[error("{0} is malformed: {1}")]
    Malformed(&'static str, &'static str),
}

/// One fact of a message's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub act: Act,
    /// The id of the fact's event.
    pub event: String,
    pub seq: u64,
}

/// One message and what became of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub message_id: String,
    /// The id and `seq` of its `protoledger/message` fact.
    pub event: String,
    pub seq: u64,
    /// The message as queued.
    pub text: String,
    pub meta: Obj,
    pub state: State,
    /// Every `protoledger/delivered`, `protoledger/cancelled` and
    /// `protoledger/withdrawn` fact that lists it, in `seq` order.
    pub history: Vec<Step>,
}

/// Every message of a ledger, derived by [`state`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Delivery {
    /// In the order they were queued (`seq` of their `protoledger/message`).
    pub messages: Vec<Message>,
    /// Delivery facts the writer would have refused where they stand: each
    /// event id with why. They change no state (SPEC §11).
    pub ignored: Vec<(String, DeliveryError)>,
    index: HashMap<String, usize>,
}

impl Delivery {
    pub fn get(&self, message_id: &str) -> Option<&Message> {
        self.index.get(message_id).map(|i| &self.messages[*i])
    }

    /// The state of `message_id`, when a `protoledger/message` names it.
    pub fn state_of(&self, message_id: &str) -> Option<State> {
        self.get(message_id).map(|m| m.state)
    }

    /// **Should send** (SPEC §11.3): every message queued and neither
    /// delivered nor withdrawn (the `pending` and the `cancelled` ones), in
    /// queue order.
    pub fn should_send(&self) -> Vec<&Message> {
        self.messages.iter().filter(|m| m.state.owed()).collect()
    }

    /// **Sent**: the `delivered` messages, in queue order.
    pub fn sent(&self) -> Vec<&Message> {
        self.messages.iter().filter(|m| m.state == State::Delivered).collect()
    }

    /// Check a `protoledger/message` for `message_id` against this state.
    fn check_message(&self, message_id: &str) -> Result<(), DeliveryError> {
        if self.index.contains_key(message_id) {
            return Err(DeliveryError::Duplicate(message_id.to_string()));
        }
        Ok(())
    }

    /// Check an `act` fact listing `ids` against this state: the refusals of
    /// SPEC §11.3.
    fn check_act(&self, act: Act, ids: &[String]) -> Result<(), DeliveryError> {
        let ty = act.fact();
        if ids.is_empty() {
            return Err(DeliveryError::Empty(ty));
        }
        let mut seen = HashSet::new();
        for id in ids {
            if !seen.insert(id.as_str()) {
                return Err(DeliveryError::Repeated(ty, id.clone()));
            }
            let Some(m) = self.get(id) else {
                return Err(DeliveryError::Unknown(ty, id.clone()));
            };
            if !m.state.owed() {
                return Err(DeliveryError::Final(ty, id.clone(), m.state));
            }
        }
        Ok(())
    }

    fn apply_message(&mut self, e: &Event, message_id: String, text: String, meta: Obj) {
        self.index.insert(message_id.clone(), self.messages.len());
        self.messages.push(Message { message_id, event: e.id.clone(), seq: e.seq, text, meta, state: State::Pending, history: Vec::new() });
    }

    fn apply_act(&mut self, e: &Event, act: Act, ids: &[String]) {
        for id in ids {
            let m = &mut self.messages[self.index[id.as_str()]];
            m.state = act.to();
            m.history.push(Step { act, event: e.id.clone(), seq: e.seq });
        }
    }
}

/// The `data` of a delivery fact, parsed.
enum Fact {
    Message { message_id: String, text: String, meta: Obj },
    Act { act: Act, ids: Vec<String> },
}

fn parse(e: &Event, ty: &'static str) -> Result<Fact, DeliveryError> {
    let data = e.body.get("data").and_then(Value::as_object).ok_or(DeliveryError::Malformed(ty, "data is not an object"))?;
    if ty == MESSAGE {
        let message_id = data.get("message_id").and_then(Value::as_str).ok_or(DeliveryError::Malformed(ty, "message_id is not a string"))?;
        let text = data.get("text").and_then(Value::as_str).ok_or(DeliveryError::Malformed(ty, "text is not a string"))?;
        let meta = match data.get("meta") {
            None => Obj::new(),
            Some(v) => v.as_object().cloned().ok_or(DeliveryError::Malformed(ty, "meta is not an object"))?,
        };
        return Ok(Fact::Message { message_id: message_id.to_string(), text: text.to_string(), meta });
    }
    let act = Act::of(ty).expect("a delivery fact type");
    let ids: Option<Vec<String>> = data.get("message_ids").and_then(Value::as_array).and_then(|a| a.iter().map(|v| v.as_str().map(str::to_string)).collect());
    let ids = ids.ok_or(DeliveryError::Malformed(ty, "message_ids is not a list of strings"))?;
    Ok(Fact::Act { act, ids })
}

/// Derive every message's delivery state and history (SPEC §11.3) from the
/// delivery facts of `ledger`, in `seq` order and from nothing else. The
/// facts are read from the whole ledger, abandoned branches included, and
/// redaction (§11.1) changes nothing here. A fact the writer would have
/// refused where it stands (a duplicate message id, an empty list, an id
/// listed twice, an unknown message, a message in a state the fact may not
/// leave, a malformed `data`) is ignored and named in
/// [`Delivery::ignored`]; the ledger stays readable.
pub fn state(ledger: &Ledger) -> Delivery {
    let mut d = Delivery::default();
    for e in &ledger.events {
        let Some(ty) = fact_type(e).filter(|t| [MESSAGE, DELIVERED, CANCELLED, WITHDRAWN].contains(t)) else {
            continue;
        };
        let checked = parse(e, ty).and_then(|f| {
            match &f {
                Fact::Message { message_id, .. } => d.check_message(message_id)?,
                Fact::Act { act, ids } => d.check_act(*act, ids)?,
            }
            Ok(f)
        });
        match checked {
            Ok(Fact::Message { message_id, text, meta }) => d.apply_message(e, message_id, text, meta),
            Ok(Fact::Act { act, ids }) => d.apply_act(e, act, &ids),
            Err(why) => d.ignored.push((e.id.clone(), why)),
        }
    }
    d
}

fn now() -> String {
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    crate::util::ms_to_rfc3339(ms)
}

fn fact_event(ty: &str, data: Value) -> Event {
    Event::new(format!("{ty}:{}", uuid::Uuid::new_v4()), Kind::App).ts(Some(now())).with("type", ty).with("data", data)
}

/// A `protoledger/message` fact for `message_id`, checked against `ledger`
/// as it is now: refused when a `protoledger/message` already names it.
pub fn message_event(ledger: &Ledger, message_id: &str, text: &str, meta: &Obj) -> Result<Event, DeliveryError> {
    state(ledger).check_message(message_id)?;
    Ok(fact_event(MESSAGE, json!({ "message_id": message_id, "text": text, "meta": meta })))
}

/// A `protoledger/delivered`, `protoledger/cancelled` or
/// `protoledger/withdrawn` fact listing `ids`, checked against `ledger` as it
/// is now: refused when `ids` is empty or lists an id twice, or when a
/// message it lists is not queued in the ledger or is `delivered` or
/// `withdrawn`. `turn` and `event` are written for `delivered` only, and
/// `reason` for the other two.
pub fn act_event(ledger: &Ledger, act: Act, ids: &[String], turn: Option<&str>, event: Option<&str>, reason: &str) -> Result<Event, DeliveryError> {
    state(ledger).check_act(act, ids)?;
    let data = match act {
        Act::Delivered => {
            let mut d = json!({ "message_ids": ids });
            if let Some(t) = turn {
                d["turn"] = json!(t);
            }
            if let Some(ev) = event {
                d["event"] = json!(ev);
            }
            d
        }
        Act::Cancelled | Act::Withdrawn => json!({ "message_ids": ids, "reason": reason }),
    };
    Ok(fact_event(act.fact(), data))
}

/// Queue `message_id` in the writer's ledger: append one
/// `protoledger/message {message_id, text, meta}` fact, checked under the
/// writer's lock. Refused, appending nothing, when the ledger already holds
/// a message with that id ([`DeliveryError::Duplicate`]).
pub fn message(ap: &mut Appender, message_id: &str, text: &str, meta: &Obj) -> Result<Appended> {
    let e = message_event(ap.ledger(), message_id, text, meta)?;
    ap.append(e)
}

/// Record that `ids` were delivered to the model, by the turn `turn` and the
/// prompt event `event` (not checked: the prompt may arrive later, by sync).
/// Refused, appending nothing, as [`act_event`] says.
pub fn delivered(ap: &mut Appender, ids: &[String], turn: Option<&str>, event: Option<&str>) -> Result<Appended> {
    let e = act_event(ap.ledger(), Act::Delivered, ids, turn, event, "")?;
    ap.append(e)
}

/// Record that one attempt to deliver exactly `ids` did not send them. They
/// stay owed (SPEC §11.3). Refused, appending nothing, as [`act_event`] says.
pub fn cancelled(ap: &mut Appender, ids: &[String], reason: &str) -> Result<Appended> {
    let e = act_event(ap.ledger(), Act::Cancelled, ids, None, None, reason)?;
    ap.append(e)
}

/// Retract `ids`: they are no longer owed. Refused, appending nothing, as
/// [`act_event`] says.
pub fn withdrawn(ap: &mut Appender, ids: &[String], reason: &str) -> Result<Appended> {
    let e = act_event(ap.ledger(), Act::Withdrawn, ids, None, None, reason)?;
    ap.append(e)
}

#[cfg(feature = "store")]
impl crate::store::Store {
    /// [`message`] through `agent`'s one writer in the store.
    pub fn message(&mut self, agent: &str, message_id: &str, text: &str, meta: &Obj) -> Result<Appended> {
        self.append_checked(agent, |l| message_event(l, message_id, text, meta))
    }

    /// [`delivered`] through `agent`'s one writer in the store.
    pub fn delivered(&mut self, agent: &str, ids: &[String], turn: Option<&str>, event: Option<&str>) -> Result<Appended> {
        self.append_checked(agent, |l| act_event(l, Act::Delivered, ids, turn, event, ""))
    }

    /// [`cancelled`] through `agent`'s one writer in the store.
    pub fn cancelled(&mut self, agent: &str, ids: &[String], reason: &str) -> Result<Appended> {
        self.append_checked(agent, |l| act_event(l, Act::Cancelled, ids, None, None, reason))
    }

    /// [`withdrawn`] through `agent`'s one writer in the store.
    pub fn withdrawn(&mut self, agent: &str, ids: &[String], reason: &str) -> Result<Appended> {
        self.append_checked(agent, |l| act_event(l, Act::Withdrawn, ids, None, None, reason))
    }

    /// [`state`] of `agent`'s ledger, as its one writer in the store holds it.
    pub fn delivery(&mut self, agent: &str) -> Result<Delivery> {
        Ok(state(self.held(agent)?))
    }
}
