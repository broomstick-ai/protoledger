//! Routes, request and response shapes (docs/SERVE.md is the reference).

use super::after;
use super::engine::{Backfill, Engine, From, Handle};
use crate::store::{AgentInfo, BindingRow, Damaged, DamagedLedger, Published};
use crate::sync;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::PathBuf;
use tokio::sync::mpsc::UnboundedReceiver;

/// Default page size of `/v1/agents/{id}/events`.
const DEFAULT_LIMIT: usize = 1000;

pub(crate) fn router(h: Handle) -> Router {
    Router::new()
        .route("/v1/subscribe", get(subscribe))
        .route("/v1/agents", get(agents))
        .route("/v1/agents/{id}", get(agent))
        .route("/v1/agents/{id}/events", get(events))
        .route("/v1/agents/{id}/children", get(children))
        .route("/v1/bindings", get(bindings))
        .route("/v1/import", post(import))
        .route("/v1/project", post(project))
        .route("/v1/attach", post(attach))
        .route("/v1/unbind", post(unbind))
        .fallback(|| async { ApiError::new(StatusCode::NOT_FOUND, "no such endpoint") })
        .with_state(h)
}

// ---- errors ----

/// Every error is `{"error": "<message>"}` with a status.
pub(crate) struct ApiError {
    status: StatusCode,
    msg: String,
}

impl ApiError {
    fn new(status: StatusCode, msg: impl Into<String>) -> ApiError {
        ApiError { status, msg: msg.into() }
    }
    fn bad(msg: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::BAD_REQUEST, msg)
    }
    fn not_found(msg: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::NOT_FOUND, msg)
    }
    /// The store refused the operation (a projection that does not come back
    /// as itself, a location already bound, an unreadable input, ...).
    /// A refusal because the ledger it reaches is damaged is a 409, as
    /// [`ApiError::reading`] answers it.
    fn refused(e: anyhow::Error) -> ApiError {
        if e.downcast_ref::<DamagedLedger>().is_some() {
            return ApiError::new(StatusCode::CONFLICT, format!("{e:#}"));
        }
        ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}"))
    }
    /// A read of the store failed: 409 when the store refused it because
    /// the ledger is damaged (the store's state, not the request, is what is
    /// wrong), else 500.
    fn reading(e: anyhow::Error) -> ApiError {
        if e.downcast_ref::<DamagedLedger>().is_some() {
            return ApiError::new(StatusCode::CONFLICT, format!("{e:#}"));
        }
        ApiError::internal(e)
    }
    fn internal(e: anyhow::Error) -> ApiError {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.msg }))).into_response()
    }
}

type Answer = Result<Json<Value>, ApiError>;

/// Run `f` on the engine; an engine that is gone is a 500.
async fn on_engine<T: Send + 'static>(h: &Handle, f: impl FnOnce(&mut Engine) -> Result<T, ApiError> + Send + 'static) -> Result<T, ApiError> {
    h.call(f).await.map_err(ApiError::internal)?
}

fn body<T: for<'de> Deserialize<'de>>(b: Result<Json<T>, axum::extract::rejection::JsonRejection>) -> Result<T, ApiError> {
    b.map(|Json(t)| t).map_err(|e| ApiError::bad(e.body_text()))
}

// ---- shapes ----

pub(crate) fn agent_json(a: &AgentInfo) -> Value {
    json!({
        "id": a.id,
        "path": a.path,
        "last_seq": a.last_seq,
        "byte_len": a.byte_len,
        "size": a.size,
        "mtime_ns": a.mtime_ns,
        "created_at": a.created_at,
        "source": a.source,
        "parent": a.parent,
        "links": a.links,
        "title": a.title,
    })
}

/// A damaged agent's row in a listing: its id, its ledger and why, and none
/// of the counts the index read before the damage (as `store ls` marks it).
fn damaged_json(a: &AgentInfo, d: &Damaged) -> Value {
    json!({ "id": a.id, "path": a.path, "damaged": d.to_string() })
}

/// An agent's row in a listing, marked when its ledger is damaged.
fn listed(e: &Engine, a: &AgentInfo) -> Value {
    match e.store.damaged_agent(&a.id) {
        Some(d) => damaged_json(a, &d),
        None => agent_json(a),
    }
}

pub(crate) fn binding_json(b: &BindingRow) -> Value {
    json!({
        "binding": b.binding,
        "agent": b.agent,
        "format": b.format,
        "variant": b.variant,
        "location": b.location,
        "native_id": b.native_id,
        "mode": b.mode,
        "active": b.active,
        "ended": b.ended,
    })
}

/// SPEC §9.4: `{agent, seq, offset, event}`.
pub(crate) fn published_json(p: &Published) -> Value {
    json!({ "agent": p.agent, "seq": p.seq, "offset": p.offset, "event": p.event })
}

/// The agent `id`: 404 when the store does not know it, 409 when its ledger
/// is damaged (every request naming it is refused, naming it).
fn known(e: &Engine, id: &str) -> Result<AgentInfo, ApiError> {
    if let Some(d) = e.store.damaged_agent(id) {
        return Err(ApiError::reading(anyhow::Error::new(DamagedLedger(d))));
    }
    e.store.agent(id).map_err(ApiError::internal)?.ok_or_else(|| ApiError::not_found(format!("no agent {id:?}")))
}

// ---- queries ----

async fn agents(State(h): State<Handle>) -> Answer {
    on_engine(&h, |e| {
        let v: Vec<Value> = e.store.agents().map_err(ApiError::internal)?.iter().map(|a| listed(e, a)).collect();
        Ok(Json(Value::Array(v)))
    })
    .await
}

async fn agent(State(h): State<Handle>, UrlPath(id): UrlPath<String>) -> Answer {
    on_engine(&h, move |e| Ok(Json(agent_json(&known(e, &id)?)))).await
}

#[derive(Deserialize)]
struct EventsQuery {
    from: Option<u64>,
    limit: Option<usize>,
}

async fn events(State(h): State<Handle>, UrlPath(id): UrlPath<String>, q: Result<Query<EventsQuery>, axum::extract::rejection::QueryRejection>) -> Answer {
    let Query(q) = q.map_err(|e| ApiError::bad(e.body_text()))?;
    let from = q.from.unwrap_or(0);
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT);
    on_engine(&h, move |e| {
        known(e, &id)?;
        // The index's query is inclusive of its first seq, so ask from `from`
        // itself (one row more than a page) and let the resume rule decide.
        let rows = e.store.events(&id, from, limit.saturating_add(1)).map_err(ApiError::internal)?;
        let mut out = Vec::with_capacity(rows.len().min(limit));
        for r in rows.into_iter().filter(|r| after(from, r.seq)).take(limit) {
            let ev = e.store.event_at(&id, r.seq).map_err(ApiError::internal)?;
            let ev = ev.ok_or_else(|| ApiError::internal(anyhow::anyhow!("{id} seq {} vanished", r.seq)))?;
            out.push(published_json(&Published { agent: id.clone(), seq: r.seq, offset: r.offset, event: ev }));
        }
        Ok(Json(Value::Array(out)))
    })
    .await
}

async fn children(State(h): State<Handle>, UrlPath(id): UrlPath<String>) -> Answer {
    on_engine(&h, move |e| {
        known(e, &id)?;
        let v: Vec<Value> = e.store.children(&id).map_err(ApiError::internal)?.iter().map(|a| listed(e, a)).collect();
        Ok(Json(Value::Array(v)))
    })
    .await
}

async fn bindings(State(h): State<Handle>) -> Answer {
    on_engine(&h, |e| {
        let v: Vec<Value> = e.store.bindings().map_err(ApiError::internal)?.iter().map(binding_json).collect();
        Ok(Json(Value::Array(v)))
    })
    .await
}

// ---- operations ----

#[derive(Deserialize)]
struct ImportReq {
    format: String,
    path: PathBuf,
}

async fn import(State(h): State<Handle>, b: Result<Json<ImportReq>, axum::extract::rejection::JsonRejection>) -> Answer {
    let r = body(b)?;
    on_engine(&h, move |e| {
        let rep = e.store.import(&r.format, &r.path).map_err(ApiError::refused)?;
        Ok(Json(json!({
            "format": rep.format,
            "variant": rep.variant,
            "created": rep.created,
            "extended": rep.extended,
            "events_appended": rep.events_appended,
            "events_skipped": rep.events_skipped,
            "losses": rep.losses,
        })))
    })
    .await
}

#[derive(Deserialize)]
struct ProjectReq {
    agent: String,
    format: String,
    dir: PathBuf,
}

async fn project(State(h): State<Handle>, b: Result<Json<ProjectReq>, axum::extract::rejection::JsonRejection>) -> Answer {
    let r = body(b)?;
    on_engine(&h, move |e| {
        known(e, &r.agent)?;
        let row = sync::project(&mut e.store, &r.agent, &r.format, &r.dir).map_err(ApiError::refused)?;
        e.rebind().map_err(ApiError::internal)?;
        Ok(Json(binding_json(&row)))
    })
    .await
}

#[derive(Deserialize)]
struct AttachReq {
    format: String,
    location: PathBuf,
}

async fn attach(State(h): State<Handle>, b: Result<Json<AttachReq>, axum::extract::rejection::JsonRejection>) -> Answer {
    let r = body(b)?;
    on_engine(&h, move |e| {
        let (row, synced) = sync::attach(&mut e.store, &r.format, &r.location).map_err(ApiError::refused)?;
        e.rebind().map_err(ApiError::internal)?;
        Ok(Json(json!({
            "binding": binding_json(&row),
            "created": synced.created,
            "events_appended": synced.appended.len(),
        })))
    })
    .await
}

#[derive(Deserialize)]
struct UnbindReq {
    binding: String,
}

async fn unbind(State(h): State<Handle>, b: Result<Json<UnbindReq>, axum::extract::rejection::JsonRejection>) -> Answer {
    let r = body(b)?;
    on_engine(&h, move |e| {
        let row = e.store.binding(&r.binding).map_err(ApiError::internal)?.ok_or_else(|| ApiError::not_found(format!("no binding {:?}", r.binding)))?;
        if !row.active {
            return Err(ApiError::new(StatusCode::CONFLICT, format!("binding {} is not active ({})", row.binding, row.ended.as_deref().unwrap_or("?"))));
        }
        sync::unbind(&mut e.store, &r.binding).map_err(ApiError::refused)?;
        e.rebind().map_err(ApiError::internal)?;
        let row = e
            .store
            .binding(&r.binding)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::internal(anyhow::anyhow!("binding {} vanished after unbind", r.binding)))?;
        Ok(Json(binding_json(&row)))
    })
    .await
}

// ---- events out ----

#[derive(Deserialize)]
struct SubscribeQuery {
    from: Option<String>,
}

/// `all`, or `<agent>:<seq>` pairs separated by commas. The seq is split off
/// at the last colon, so an agent id may itself hold colons.
pub(crate) fn parse_from(s: &str) -> Result<From, String> {
    if s == "all" {
        return Ok(From::All);
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for part in s.split(',') {
        let (agent, seq) = part.rsplit_once(':').ok_or_else(|| format!("{part:?} is not <agent>:<seq>"))?;
        if agent.is_empty() {
            return Err(format!("{part:?}: empty agent id"));
        }
        let seq: u64 = seq.parse().map_err(|_| format!("{part:?}: {seq:?} is not a seq"))?;
        if !seen.insert(agent.to_string()) {
            return Err(format!("agent {agent:?} is listed twice"));
        }
        out.push((agent.to_string(), seq));
    }
    Ok(From::Agents(out))
}

async fn subscribe(
    State(h): State<Handle>,
    q: Result<Query<SubscribeQuery>, axum::extract::rejection::QueryRejection>,
    ws: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Response {
    let r: Result<Response, ApiError> = async {
        let Query(q) = q.map_err(|e| ApiError::bad(e.body_text()))?;
        let from = q.from.ok_or_else(|| ApiError::bad("from is required: <agent>:<seq>,... or all"))?;
        let from = parse_from(&from).map_err(ApiError::bad)?;
        let ws = ws.map_err(|e| ApiError::bad(e.body_text()))?;
        let (rx, backfill) = on_engine(&h, move |e| e.subscribe(&from).map_err(ApiError::reading)).await?;
        Ok(ws.on_upgrade(move |socket| stream(socket, rx, backfill)))
    }
    .await;
    r.unwrap_or_else(IntoResponse::into_response)
}

async fn send(socket: &mut WebSocket, p: &Published) -> bool {
    socket.send(Message::Text(published_json(p).to_string().into())).await.is_ok()
}

/// The backfill first, agent by agent, then live events as they come.
async fn stream(mut socket: WebSocket, mut rx: UnboundedReceiver<Published>, backfill: Vec<Backfill>) {
    for b in backfill {
        let read = tokio::task::spawn_blocking(move || b.read()).await;
        match read {
            Ok(Ok(evs)) => {
                for p in &evs {
                    if !send(&mut socket, p).await {
                        return;
                    }
                }
            }
            Ok(Err(e)) => return close(socket, &format!("backfill: {e:#}")).await,
            Err(e) => return close(socket, &format!("backfill: {e}")).await,
        }
    }
    loop {
        tokio::select! {
            p = rx.recv() => match p {
                Some(p) => if !send(&mut socket, &p).await { return },
                None => return close(socket, "the server is stopping").await,
            },
            m = socket.recv() => match m {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                Some(Ok(_)) => {} // the protocol has nothing for a client to say
            },
        }
    }
}

async fn close(mut socket: WebSocket, why: &str) {
    let frame = axum::extract::ws::CloseFrame { code: 1011, reason: why.chars().take(120).collect::<String>().into() };
    let _ = socket.send(Message::Close(Some(frame))).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    /// A store refusing a damaged ledger is a 409; any other failure of a
    /// read of the store stays a 500, even one wrapped in context, and a
    /// refusal wrapped in context is still a 409. The errors are the store's
    /// own: one reader refused for damage, one for an agent it does not know.
    #[test]
    fn a_damaged_ledger_is_409_and_every_other_store_error_stays_500() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");
        let mut s = Store::open(&root).unwrap();
        s.import("protoledger", &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/protoledger/minimal/native")).unwrap();
        let path = root.join(s.agent("hello-1").unwrap().unwrap().path);
        drop(s);
        let text = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<&str> = text.lines().collect();
        lines[1] = "{this line is not json";
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let s = Store::open(&root).unwrap();

        let damaged = s.reader("hello-1").unwrap_err();
        assert!(damaged.downcast_ref::<DamagedLedger>().is_some(), "{damaged:#}");
        let other = s.reader("nobody").unwrap_err();
        assert!(other.downcast_ref::<DamagedLedger>().is_none(), "{other:#}");

        let r = ApiError::reading(damaged);
        assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.msg);
        assert!(r.msg.contains("damaged") && r.msg.contains("hello-1"), "{}", r.msg);
        let r = ApiError::reading(s.reader("hello-1").unwrap_err().context("subscribing"));
        assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.msg);
        let r = ApiError::reading(other);
        assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR, "{}", r.msg);
        let r = ApiError::reading(s.reader("nobody").unwrap_err().context("subscribing"));
        assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR, "{}", r.msg);
    }
}
